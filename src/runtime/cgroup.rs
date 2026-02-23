use std::fs;
use std::io::Write;
use std::path::PathBuf;

use oci_spec::runtime::Spec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupManager {
    root_path: PathBuf,
}
impl CgroupManager {
    pub fn new(container_id: &str) -> Self {
        // Cgroup V2 的默认挂载路径
        let path = PathBuf::from("/sys/fs/cgroup").join(container_id);
        Self { root_path: path }
    }
    pub fn apply_limits(&self, spec: &Spec, pid: u32) -> Result<(), std::io::Error> {
        // 1. 确保父级开启控制控制器 (v2 逻辑)
        Self::enable_controllers()?;

        // 2. 创建容器 cgroup 目录
        if !self.root_path.exists() {
            fs::create_dir_all(&self.root_path)?;
        }

        // 3. 解析并应用资源限制
        if let Some(linux) = spec.linux() {
            if let Some(resources) = linux.resources() {
                // 处理内存限制
                if let Some(memory) = resources.memory() {
                    if let Some(limit) = memory.limit() {
                        self.write_cgroup_file("memory.max", &limit.to_string())?;
                    }
                    if let Some(reservation) = memory.reservation() {
                        self.write_cgroup_file("memory.low", &reservation.to_string())?;
                    }
                }

                // 处理 CPU 限制
                if let Some(cpu) = resources.cpu() {
                    // Cgroup v2 使用 cpu.max: "period quota"
                    if let (Some(quota), Some(period)) = (cpu.quota(), cpu.period()) {
                        let val = format!("{} {}", quota, period);
                        self.write_cgroup_file("cpu.max", &val)?;
                    }
                    if let Some(shares) = cpu.shares() {
                        // v2 中 shares 对应 cpu.weight (公式通常为 1 + ((shares - 2) * 9999) / 262142)
                        self.write_cgroup_file("cpu.weight", &shares.to_string())?;
                    }
                }

                // 处理 PIDs 限制
                if let Some(pids) = resources.pids() {
                    self.write_cgroup_file("pids.max", &pids.limit().to_string())?;
                }
            }
        }

        // 4. 最后将 PID 写入进程文件，正式加入控制
        self.write_cgroup_file("cgroup.procs", &pid.to_string())?;

        Ok(())
    }

    // 辅助函数：简化写入操作
    fn write_cgroup_file(&self, filename: &str, value: &str) -> Result<(), std::io::Error> {
        let path = self.root_path.join(filename);
        fs::write(path, value)
    }

    // 静态函数：启用控制器
    fn enable_controllers() -> Result<(), std::io::Error> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open("/sys/fs/cgroup/cgroup.subtree_control")?;
        // 尝试开启常用控制器
        let _ = f.write_all(b"+cpu +cpuset +memory +pids +io");
        Ok(())
    }
}
