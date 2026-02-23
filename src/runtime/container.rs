use crate::runtime::error::RuntimeError;
use crate::runtime::{action::Action, state::ContainerState};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, chdir, execve, fork, pipe};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;

// ===== 核心依赖导入（nix 0.31.1 适配）=====
use nix::mount::{MntFlags, MsFlags, mount, umount, umount2};
use nix::pty::{OpenptyResult, openpty};
use nix::sched::{CloneFlags, unshare};
use nix::sys::stat::{Mode, SFlag, mknod};
use nix::sys::termios::{
    LocalFlags, SetArg, SpecialCharacterIndices, Termios, tcgetattr, tcsetattr,
};
use nix::unistd::{close, dup2, pivot_root, setsid};

// ===== 常量定义（仅保留核心必要常量）=====
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

    /// 从 OCI 状态恢复容器实例
    fn try_from(oci: OciState) -> Result<Self, Self::Error> {
        let state = match oci.status.as_str() {
            "creating" => ContainerState::Creating,
            "created" => ContainerState::Created,
            "running" => ContainerState::Running,
            "stopped" => ContainerState::Stopped,
            "paused" => ContainerState::Paused,
            other => return Err(RuntimeError::InvalidState(format!("未知状态: {}", other))),
        };

        Ok(Container {
            id: oci.id,
            bundle: PathBuf::from(oci.bundle),
            state,
            pid: oci.pid.map(|p| p as u32),
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
}

// ===== 路径工具函数（无业务逻辑,仅路径拼接）=====
fn runtime_root() -> PathBuf {
    PathBuf::from("/run/runsys")
}

fn container_dir(id: &str) -> PathBuf {
    runtime_root().join(id)
}

fn state_path(id: &str) -> PathBuf {
    container_dir(id).join("state.json")
}

fn exec_fifo_path(id: &str) -> PathBuf {
    container_dir(id).join("exec.fifo")
}

// ===== 容器核心方法实现 =====
impl Container {
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

    /// 应用容器生命周期动作（Create/Start/Kill 等）
    fn apply_action(&mut self, action: Action) -> Result<(), RuntimeError> {
        let next = self.state.apply(action.clone())?;
        if let Some(next_state) = next {
            self.state = next_state;
        }
        Ok(())
    }
    /// 创建容器(初始化目录/FIFO设置初始状态)
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

        // 初始化容器实例并保存状态
        let mut container = Container {
            id: id.clone(),
            bundle: bundle.clone(),
            state: ContainerState::Creating,
            pid: None,
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

        // 状态校验：仅 Created 状态可启动
        if self.state != ContainerState::Created {
            return Err(RuntimeError::InvalidState(format!(
                "容器必须处于 Created 状态才能启动,当前: {:?}",
                self.state
            )));
        }

        // 加载 OCI 配置
        let config_path = self.bundle.join("config.json");
        println!("[runsys]: 正在读取 OCI 配置: {:?}", config_path);

        if !config_path.exists() {
            println!("[runsys]: 错误 - 找不到 config.json");
            return Err(RuntimeError::ConfigNotFound(config_path));
        }

        let config_content = fs::read_to_string(&config_path)?;
        let spec: oci_spec::runtime::Spec = serde_json::from_str(&config_content)?;
        let process = spec.process().as_ref().ok_or_else(|| {
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

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                println!("[runsys]: 父进程继续执行, 子进程 PID: {}", child);
                let _ = close(sync_read_fd);
                self.handle_parent(child, sync_write_fd.into_raw_fd())?;
                Ok(())
            }
            Ok(ForkResult::Child) => {
                let _ = close(sync_write_fd);
                // 子进程逻辑
                self.handle_child(&args, &env, cwd, sync_read_fd.into_raw_fd())?;
                std::process::exit(1);
            }
            Err(e) => {
                println!("[runsys]: Fork 失败: {:?}", e);
                Err(RuntimeError::NixError(e))
            }
        }
    }

    /// 父进程逻辑（终端管理/子进程监控）
    fn handle_parent(&mut self, child: Pid, sync_write_fd: RawFd) -> Result<(), RuntimeError> {
        // 更新容器状态
        self.pid = Some(child.as_raw() as u32);
        self.apply_action(Action::Start)?;
        println!("[Parents]:容器启动成功,PID: {}", child.as_raw());
        self.save()?;
        // 发送容器信号
        let mut writer = unsafe { File::from_raw_fd(sync_write_fd) };
        writer.write_all(b"1").map_err(RuntimeError::IoError)?;
        drop(writer);
        println!("[Parents]:信号已发送,容器正式运行。");
        // 等待容器运行结束
        match waitpid(child, None) {
            Ok(status) => println!("[Parents]:容器退出状态: {:?}", status),
            Err(e) => return Err(RuntimeError::NixError(e)),
        }
        // 容器结束后,清理状态
        self.apply_action(Action::Pause)?;
        self.save()?;
        Ok(())
    }

    /// 子进程逻辑（容器内执行）
    fn handle_child(
        &self,
        args: &[CString],
        env: &[CString],
        cwd: &str,
        sync_read_fd: RawFd,
    ) -> Result<(), RuntimeError> {
        // 1. 隔离命名空间
        unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWIPC)?;
        println!("[Child]: 命名空间隔离完成 (NS, UTS, IPC)");

        // 2. 挂载传播设置为私有
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_PRIVATE | MsFlags::MS_REC,
            None::<&str>,
        )?;

        // 3. 准备 Rootfs 挂载点
        let rootfs_path = self.bundle.join("rootfs");
        mount(
            Some(rootfs_path.as_path()),
            rootfs_path.as_path(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )?;

        // 4. 切换根目录 (Pivot Root)
        chdir(rootfs_path.as_path())?;
        let put_old = rootfs_path.join(".old_root");
        fs::create_dir_all(&put_old).unwrap_or_default();
        pivot_root(".", put_old.as_path())?;

        // 5. 切换到新根并清理旧根
        chdir("/")?;
        let old_root_path = Path::new("/.old_root");
        umount2(old_root_path, MntFlags::MNT_DETACH)?;
        fs::remove_dir(old_root_path).unwrap_or_default();
        println!("[Child]: 根目录已成功切换至 rootfs");

        // 6. 挂载虚拟文件系统
        fs::create_dir_all("/proc").unwrap_or_default();
        mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )?;
        println!("[Child]: /proc 系统文件挂载完成");

        // 7. 同步等待父进程信号
        println!("[Child]: 正在等待父进程同步信号...");
        let mut reader = unsafe { File::from_raw_fd(sync_read_fd) };
        let mut buf = [0; 1];
        let _ = reader.read_exact(&mut buf);
        drop(reader);
        println!("[Child]: 收到同步信号,准备chdir和execve");

        // 8. 变身执行目标程序
        println!("[Child]: 切换工作目录至: {}", cwd);
        chdir(cwd)?;

        println!("[Child]: 正在执行 execve -> {:?}", args[0]);
        execve(&args[0], args, env).map_err(RuntimeError::NixError)?;
        //execve(&CString::new("/bin/sh").unwrap(), args, env).map_err(RuntimeError::NixError)?;

        Ok(())
    }
}
