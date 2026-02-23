use crate::runtime::cgroup::CgroupManager;
use crate::runtime::error::RuntimeError;
use crate::runtime::{action::Action, state::ContainerState};
use nix::ioctl_write_int_bad;
use nix::mount::{MntFlags, MsFlags, mount, umount, umount2};
use nix::pty::{OpenptyResult, openpty};
use nix::sched::{CloneFlags, unshare};
use nix::sys::stat::{Mode, SFlag, mknod};
use nix::sys::termios::{
    LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios, tcgetattr, tcsetattr,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, chdir, execve, fork, pipe};
use nix::unistd::{close, dup2, pivot_root, setsid};
use oci_spec::runtime::{Mount, Spec};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;
// ===== 常量定义(仅保留核心必要常量)=====
const STDIN_FILENO: i32 = 0;
const STDOUT_FILENO: i32 = 1;
const STDERR_FILENO: i32 = 2;

// ===== OCI 状态序列化结构体 =====
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OciState {
    oci_version: String,
    id: String,
    status: String,
    pid: Option<i32>,
    bundle: String,
}

impl From<&Container> for OciState {
    /// 从容器实例转换为 OCI 标准状态
    fn from(container: &Container) -> Self {
        OciState {
            oci_version: "1.0.2".to_string(),
            id: container.id.clone(),
            status: match container.state {
                ContainerState::Creating => "creating".to_string(),
                ContainerState::Created => "created".to_string(),
                ContainerState::Running => "running".to_string(),
                ContainerState::Stopped => "stopped".to_string(),
                ContainerState::Paused => "paused".to_string(),
            },
            pid: container.pid.map(|p| p as i32),
            bundle: container.bundle.to_string_lossy().into_owned(),
        }
    }
}

impl TryFrom<OciState> for Container {
    type Error = RuntimeError;

    fn try_from(oci: OciState) -> Result<Self, Self::Error> {
        // 1. 恢复基础状态
        let state = match oci.status.as_str() {
            "creating" => ContainerState::Creating,
            "created" => ContainerState::Created,
            "running" => ContainerState::Running,
            "stopped" => ContainerState::Stopped,
            "paused" => ContainerState::Paused,
            other => return Err(RuntimeError::InvalidState(format!("未知状态: {}", other))),
        };
        let bundle_path = PathBuf::from(&oci.bundle);

        // 2. 关键点：从 bundle 重新加载当时创建时保存的 config.json
        // 或者你可以从 state 目录下的备份加载
        let config_path = bundle_path.join("config.json");
        let config_content = fs::read_to_string(&config_path)?;
        let spec: oci_spec::runtime::Spec = serde_json::from_str(&config_content)?;
        let cgroupmng = CgroupManager::new(&oci.id);

        Ok(Container {
            id: oci.id,
            bundle: bundle_path,
            state,
            pid: oci.pid.map(|p| p as u32),
            cgroupmng,
            spec, // 重新填充 spec
        })
    }
}
impl OciState {
    /// 保存 OCI 状态到文件
    pub fn save(&self, id: &str) -> Result<(), RuntimeError> {
        let path = state_path(id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    /// 从文件加载 OCI 状态
    pub fn load(id: &str) -> Result<Self, RuntimeError> {
        let path = state_path(id);
        if !path.exists() {
            return Err(RuntimeError::ContainerNotFound(id.to_string()));
        }
        let content = fs::read_to_string(&path)?;
        let oci: OciState = serde_json::from_str(&content)?;

        if oci.id != id {
            return Err(RuntimeError::IdMismatch {
                expected: id.to_string(),
                got: oci.id,
            });
        }
        Ok(oci)
    }
}

// ===== 容器核心结构体 =====
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub bundle: PathBuf,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub cgroupmng: CgroupManager,
    pub spec: oci_spec::runtime::Spec,
}

// ===== 路径工具函数(无业务逻辑,仅路径拼接)=====
fn runtime_root() -> PathBuf {
    PathBuf::from("/run/runsys")
}

fn container_dir(id: &str) -> PathBuf {
    runtime_root().join(id)
}

fn state_path(id: &str) -> PathBuf {
    container_dir(id).join("state.json")
}

// ===== 容器核心方法实现 =====
impl Container {
    /// 应用资源限制并将进程加入控制组
    pub fn apply_limits(&self, pid: u32, spec: &Spec) -> Result<(), RuntimeError> {
        println!("[Container]: 正在为 PID {} 应用 Cgroup 限制", pid);
        self.cgroupmng
            .apply_limits(spec, pid)
            .map_err(RuntimeError::CgroupError)?;
        Ok(())
    }

    /// 从状态文件加载容器
    pub fn load(id: &str) -> Result<Self, RuntimeError> {
        let oci = OciState::load(id)?;
        Container::try_from(oci)
    }

    /// 保存容器状态到文件
    pub fn save(&self) -> Result<(), RuntimeError> {
        let oci = OciState::from(self);
        oci.save(&self.id)
    }

    /// 应用容器生命周期动作(Create/Start/Kill 等)
    fn apply_action(&mut self, action: Action) -> Result<(), RuntimeError> {
        let next = self.state.apply(action.clone())?;
        if let Some(next_state) = next {
            self.state = next_state;
        }
        Ok(())
    }
    /// 创建容器(初始化目录设置初始状态)
    pub fn create(id: String, bundle: PathBuf) -> Result<Self, RuntimeError> {
        println!("[runsys]: 尝试从 bundle 创建容器 '{}': {:?}", id, bundle);

        // 校验 bundle 目录合法性
        if !bundle.is_dir() {
            println!("[runsys]: 错误 - bundle 路径不是有效目录: {:?}", bundle);
            return Err(RuntimeError::InvalidBundle(bundle));
        }

        // 创建容器运行时目录
        let cont_dir = container_dir(&id);
        if cont_dir.exists() {
            println!("[runsys]: 错误 - 容器运行时目录已存在: {:?}", cont_dir);
            return Err(RuntimeError::ContainerAlreadyExists(id.clone()));
        }
        fs::create_dir_all(&cont_dir)?;
        println!("[runsys]: 运行时目录已准备: {:?}", cont_dir);

        // 加载 OCI 配置
        let config_path = bundle.join("config.json");
        println!("[runsys]: 正在读取 OCI 配置: {:?}", config_path);

        if !config_path.exists() {
            println!("[runsys]: 错误 - 找不到 config.json");
            return Err(RuntimeError::ConfigNotFound(config_path));
        }

        let config_content = fs::read_to_string(&config_path)?;
        let spec: oci_spec::runtime::Spec = serde_json::from_str(&config_content)?;
        // 初始化容器实例并保存状态
        let mut container = Container {
            id: id.clone(),
            bundle: bundle.clone(),
            state: ContainerState::Creating,
            pid: None,
            cgroupmng: CgroupManager::new(&id),
            spec: spec,
        };
        container.apply_action(Action::Create)?;
        container.state = ContainerState::Created;
        container.save()?;

        println!("[runsys]: 容器 '{}' 状态已持久化 (state: Created)", id);
        Ok(container)
    }

    /// 启动容器(核心入口)
    pub fn start(&mut self) -> Result<(), RuntimeError> {
        println!("[runsys]: 正在启动容器 '{}'...", self.id);

        // 状态校验:仅 Created 状态可启动
        if self.state != ContainerState::Created {
            return Err(RuntimeError::InvalidState(format!(
                "容器必须处于 Created 状态才能启动,当前: {:?}",
                self.state
            )));
        }

        let process = self.spec.process().as_ref().ok_or_else(|| {
            RuntimeError::InvalidState("config.json 缺少 process 配置".to_string())
        })?;

        // 解析进程参数
        let args: Vec<CString> = process
            .args()
            .as_ref()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|s| CString::new(s.as_str()).map_err(RuntimeError::CStringError))
            .collect::<Result<_, _>>()?;

        println!("[runsys]: 解析到的执行参数: {:?}", args);

        let env: Vec<CString> = process
            .env()
            .as_ref()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|s| CString::new(s.as_str()).map_err(RuntimeError::CStringError))
            .collect::<Result<_, _>>()?;

        let cwd_raw = process.cwd().to_str().unwrap_or("/");
        let cwd = if cwd_raw.is_empty() { "/" } else { cwd_raw };
        println!("[runsys]: 设定容器工作目录: '{}'", cwd);

        // Fork 子进程
        println!("[runsys]: 准备执行 fork...");
        let (sync_read_fd, sync_write_fd) = pipe().map_err(RuntimeError::NixError)?;

        //创建PTY
        let pty_result = openpty(None, None).map_err(RuntimeError::NixError)?;
        let master_fd = pty_result.master;
        let slave_fd = pty_result.slave;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                println!("[runsys]: 父进程继续执行, 子进程 PID: {}", child);
                let _ = close(sync_read_fd);
                let _ = close(slave_fd);
                self.handle_parent(child, sync_write_fd.into_raw_fd(), master_fd)?;
                Ok(())
            }
            Ok(ForkResult::Child) => {
                let _ = close(sync_write_fd);
                let _ = close(master_fd);
                // 子进程逻辑
                self.handle_child(&args, &env, cwd, sync_read_fd.into_raw_fd(), slave_fd)?;
                std::process::exit(1);
            }
            Err(e) => {
                println!("[runsys]: Fork 失败: {:?}", e);
                Err(RuntimeError::NixError(e))
            }
        }
    }

    /// 父进程逻辑处理(负责生命周期监控与终端数据中继)
    ///
    /// 此函数运行在宿主机空间,充当容器的“监护人”。它负责在子进程准备好后发出启动信号,
    /// 并通过 PTY Master 端建立宿主机标准 I/O 与容器内部 I/O 的桥梁。
    fn handle_parent(
        &mut self,
        child: Pid,
        sync_write_fd: RawFd,
        master_fd: OwnedFd,
    ) -> Result<(), RuntimeError> {
        self.apply_limits(child.as_raw() as u32, &self.spec)?;
        // 1. 更新并持久化容器状态
        // 记录容器真正的 PID(子进程 fork 后的 PID),并将状态从 Created 变更为 Running。
        self.pid = Some(child.as_raw() as u32);
        self.apply_action(Action::Start)?;
        println!("[Parents]: 容器子进程启动成功,分配 PID: {}", child.as_raw());
        self.save()?;

        // 2. 发送容器同步启动信号
        // 通过管道向子进程写入一个字节,解除子进程在 execve 之前的阻塞等待,确保父进程配置(如持久化)先完成。
        let mut writer = unsafe { File::from_raw_fd(sync_write_fd) };
        writer.write_all(b"1").map_err(RuntimeError::IoError)?;
        drop(writer); // 关闭写端触发子进程读端 EOF 或成功读取
        println!("[Parents]: 同步信号已发送,通知容器执行业务程序。");

        // 3. 宿主机终端属性调整:切换至 Raw Mode(原始模式)
        // 目的是让宿主机终端不再解释特殊按键(如 Ctrl+C),而是将原始字符流发给父进程,
        // 再由父进程通过 PTY 转发给容器。开启 OPOST 和 ONLCR 确保父进程自身的 println! 换行正常。
        let old_termios = tcgetattr(io::stdin())?;
        let mut raw_termios = old_termios.clone();
        nix::sys::termios::cfmakeraw(&mut raw_termios);
        raw_termios.output_flags |= OutputFlags::OPOST;
        raw_termios.output_flags |= OutputFlags::ONLCR;
        tcsetattr(io::stdin(), SetArg::TCSADRAIN, &raw_termios)?;
        println!("[Parents]: 宿主机终端已切换至 Raw Mode 以支持交互。");

        // 4. 获取 PTY Master 的文件句柄用于双向通信
        let master_fd_clone = master_fd.as_raw_fd();
        let mut master_file_rx = unsafe { File::from_raw_fd(master_fd_clone) };
        let mut master_file_tx = unsafe { File::from_raw_fd(master_fd.into_raw_fd()) };

        // 线程 A: 容器输出中继 (Outbound)
        // 持续读取 PTY Master 端的数据(即容器的 stdout/stderr),并将其写入宿主机的标准输出。
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match master_file_rx.read(&mut buf) {
                    Ok(0) | Err(_) => break, // 容器关闭或出错则退出线程
                    Ok(n) => {
                        let _ = io::stdout().write_all(&buf[..n]);
                        let _ = io::stdout().flush();
                    }
                }
            }
        });

        // 线程 B: 宿主机输入中继 (Inbound)
        // 持续读取宿主机的标准输入(键盘),并将其写入 PTY Master 端(即容器的 stdin)。
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut stdin = io::stdin();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break, // 宿主机输入流关闭则退出
                    Ok(n) => {
                        if let Err(_) = master_file_tx.write_all(&buf[..n]) {
                            break;
                        }
                    }
                }
            }
        });

        // 5. 阻塞等待容器退出
        // 监视子进程生命周期,当容器内的程序执行完毕或被杀死时,此调用将返回。
        match waitpid(child, None) {
            Ok(status) => println!("\r\n[Parents]: 容器进程已结束,退出状态: {:?}", status),
            Err(e) => return Err(RuntimeError::NixError(e)),
        }

        // 6. 恢复宿主机终端原始属性
        // 非常关键:必须将终端从 Raw Mode 切换回正常的 Cooked Mode,
        // 否则用户回到宿主机 Shell 后,终端显示和输入逻辑会处于混乱状态。
        tcsetattr(io::stdin(), SetArg::TCSADRAIN, &old_termios)?;

        // 7. 清理并更新容器运行状态
        self.apply_action(Action::Pause)?;
        self.save()?;
        Ok(())
    }

    /// 子进程逻辑入口(运行于容器隔离环境内)
    ///
    /// 负责执行命名空间隔离、根文件系统切换、伪终端绑定,
    /// 并通过双重 Fork 实现 PID 命名空间的真正隔离。
    fn handle_child(
        &self,
        args: &[CString],
        env: &[CString],
        cwd: &str,
        sync_read_fd: RawFd,
        slave_fd: OwnedFd,
    ) -> Result<(), RuntimeError> {
        // 1. 创建独立的命名空间隔离环境
        // CLONE_NEWNS: 挂载隔离 | CLONE_NEWUTS: 主机名隔离 | CLONE_NEWIPC: 进程间通信隔离
        // CLONE_NEWNET: 网络栈隔离 | CLONE_NEWPID: 进程号隔离(注意:仅对子进程生效)
        unshare(
            CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWUTS
                | CloneFlags::CLONE_NEWIPC
                | CloneFlags::CLONE_NEWNET
                | CloneFlags::CLONE_NEWPID,
        )?;
        println!("[Child]: 命名空间 [NS, UTS, IPC, NET, PID] 隔离配置完成");

        // 2. 将挂载传播属性设置为私有,防止容器内的挂载操作泄露到宿主机
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_PRIVATE | MsFlags::MS_REC,
            None::<&str>,
        )?;

        // 3. 准备 Rootfs 挂载点:通过 Bind Mount 将 rootfs 目录变为挂载点,
        // 这是后续 pivot_root 能够成功执行的前提条件。
        let rootfs_path = self.bundle.join("rootfs");
        mount(
            Some(rootfs_path.as_path()),
            rootfs_path.as_path(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )?;

        // 6. 第一次挂载虚拟文件系统(可选:仅为中间进程提供环境,PID 1 进程会重新挂载)
        // let mounts = self.spec.mounts().as_ref().unwrap();

        chdir(rootfs_path.as_path())?;

        // 4. 执行 pivot_root:将当前进程的根目录切换到 rootfs。
        // .old_root 用于暂存切换前的老根文件系统,方便后续清理。
        let old_root_name = ".old_root";
        fs::create_dir_all(old_root_name).unwrap_or_default();
        pivot_root(".", old_root_name).map_err(|e| {
            eprintln!(
                "[Child Error]: pivot_root 失败. new: . , old: {} , 错误: {}",
                old_root_name, e
            );
            RuntimeError::from(e)
        })?;

        // 5. 正式进入新根目录并彻底卸载旧根文件系统,实现完全的文件系统隔离
        chdir("/")?;
        let old_root_path = Path::new("/.old_root");
        umount2(old_root_path, MntFlags::MNT_DETACH)?;
        fs::remove_dir(old_root_path).unwrap_or_default();
        println!("[Child]: 根目录 (rootfs) 切换及清理工作完成");

        // 7. 第二次 Fork:创建真正处于 PID 命名空间内的 PID 1 进程。
        // 因为 unshare(CLONE_NEWPID) 仅对调用者的后续子进程生效。
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                // 中间进程 (Intermediate Process)
                // 此时它在宿主机的视角下是普通进程,但在容器视角下是 PID 1 的父进程。
                // 它的唯一职责是担任“看守者”,等待真正的容器主进程结束。
                match waitpid(child, None) {
                    Ok(_) => std::process::exit(0),
                    Err(_) => std::process::exit(1),
                }
            }
            Ok(ForkResult::Child) => {
                // 真正的容器主进程 (Final Container Process)
                // 在此代码块内,该进程在当前 PID 命名空间中的 PID 将恒为 1。

                // 8. 重新挂载 /proc:核心步骤。
                // procfs 是进程信息的实时视图,只有在此处挂载,ps 等工具才会仅显示容器内进程。
                fs::create_dir_all("/proc").unwrap_or_default();
                mount(
                    Some("proc"),
                    "/proc",
                    Some("proc"),
                    MsFlags::empty(),
                    None::<&str>,
                )?;
                println!("[Child]: 容器 PID 1 /proc 视图挂载完成");
                //sysfs
                fs::create_dir_all("/sys").unwrap_or_default();
                mount(
                    Some("sysfs"),
                    "/sys",
                    Some("sysfs"),
                    MsFlags::MS_RDONLY
                        | MsFlags::MS_NOSUID
                        | MsFlags::MS_NOEXEC
                        | MsFlags::MS_NODEV,
                    None::<&str>,
                )?;
                println!("[Child]: 容器 PID 1 /sys 视图挂载完成 (Read-Only)");

                // 9. 信号同步:阻塞等待父进程完成配置逻辑(如 Cgroups 绑定或状态持久化)
                println!("[Child]: 正在等待父进程同步信号...");
                {
                    let mut reader = unsafe { File::from_raw_fd(sync_read_fd) };
                    let mut buf = [0; 1];
                    let _ = reader.read_exact(&mut buf);
                    drop(reader);
                }
                println!("[Child]: 收到同步信号,开始绑定控制终端并准备执行程序");

                // 10. 设置控制终端 (PTY):
                // setsid 创建新会话,TIOCSCTTY 将 PTY Slave 强制绑定为当前进程的控制终端。
                ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);
                let _ = nix::unistd::close(0);
                let _ = nix::unistd::close(1);
                let _ = nix::unistd::close(2);
                setsid().map_err(RuntimeError::NixError)?;
                unsafe {
                    tiocsctty(slave_fd.as_raw_fd(), 0)?;
                }

                // 11. 标准流重定向:将容器的标准输入、输出、错误重定向到 PTY Slave 端。
                // 这样宿主机通过 Master 端就能与容器内的程序进行交互。
                let mut stdin_fd = unsafe { OwnedFd::from_raw_fd(0) };
                let mut stdout_fd = unsafe { OwnedFd::from_raw_fd(1) };
                let mut stderr_fd = unsafe { OwnedFd::from_raw_fd(2) };
                dup2(&slave_fd, &mut stdin_fd)?;
                dup2(&slave_fd, &mut stdout_fd)?;
                dup2(&slave_fd, &mut stderr_fd)?;

                // 清理不再需要的 Slave 文件描述符副本
                let raw = slave_fd.as_raw_fd();
                if raw > 2 {
                    let _ = nix::unistd::close(raw);
                }

                // 12. 设置容器内主机名
                let hostname = "mzh-container";
                let hostname_ptr = hostname.as_ptr() as *const libc::c_char;
                let hostname_len = hostname.len();
                println!("[Child]: 设置 Hostname 为 '{}'", hostname);
                unsafe {
                    if libc::sethostname(hostname_ptr, hostname_len) != 0 {
                        return Err(RuntimeError::IoError(std::io::Error::last_os_error()));
                    }
                }

                // 13. 切换到容器定义的工作目录并启动目标程序
                println!("[Child]: 切换工作目录至: {}", cwd);
                chdir(cwd)?;

                println!("[Child]: 正在执行 execve -> {:?}", args[0]);
                // 使用 execve 替换当前进程映像,程序正式从运行时切换为目标应用。
                execve(&args[0], args, env).map_err(RuntimeError::NixError)?;

                Ok(())
            }
            Err(e) => Err(RuntimeError::NixError(e)),
        }
    }
}
