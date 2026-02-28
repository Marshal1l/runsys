# runsys

一个使用 Rust 编写的简单 OCI 兼容容器运行时 (Container Runtime)。

## 项目简介

`runsys` 是一个轻量级的容器运行时实现，兼容 [OCI Runtime Spec](https://github.com/opencontainers/runtime-spec)。它允许你创建和运行符合 OCI 标准的容器，支持完整的 Linux 容器隔离技术。

### 核心特性

- **OCI 标准兼容**：支持标准的 OCI Bundle 格式和 `config.json` 配置
- **完整的命名空间隔离**：支持 PID、Network、IPC、UTS、Mount 命名空间隔离
- **根文件系统隔离**：使用 `pivot_root` 实现容器的独立文件系统视图
- **Cgroup V2 支持**：支持对容器的 CPU、内存、PIDs 等资源进行限制
- **伪终端 (PTY) 支持**：为容器提供交互式终端体验
- **状态持久化**：容器状态保存在 `/run/runsys/<container-id>/state.json`

## 项目结构

```
.
├── Cargo.toml              # Rust 项目配置
├── src/
│   ├── main.rs             # 程序入口
│   ├── cli.rs              # 命令行接口 (CLI)
│   └── runtime/            # 容器运行时核心
│       ├── mod.rs          # 模块导出
│       ├── container.rs    # 容器核心实现
│       ├── state.rs        # 容器状态管理
│       ├── action.rs       # 容器生命周期操作
│       ├── cgroup.rs       # Cgroup V2 资源管理
│       └── error.rs        # 错误处理定义
└── busybox-test/           # 测试用的 OCI Bundle
    ├── config.json         # OCI 运行时配置
    └── rootfs/             # 容器根文件系统 (BusyBox)
```

## 依赖项

```toml
[dependencies]
anyhow = "1.0"           # 错误处理
clap = { version = "4.5", features = ["derive"] }  # CLI 解析
libc = "0.2"             # C 库绑定
log = "0.4"              # 日志记录
nix = { version = "0.31", features = ["process", "fs", "mount", "term", "sched", "ioctl", "poll"] }  # Linux 系统调用
oci-spec = "0.9"         # OCI 规范解析
serde = { version = "1.0", features = ["derive"] }  # 序列化
serde_json = "1.0"       # JSON 处理
thiserror = "2.0"        # 错误定义
```

## 构建与安装

### 前置要求

- Rust 1.85+ (Edition 2024)
- Linux 操作系统 (需要较新的内核支持 Cgroup V2)
- root 权限 (运行容器需要)

### 构建步骤

```bash
# 克隆仓库
git clone <repository-url>
cd runsys

# 构建发布版本
cargo build --release

# 可执行文件位于 target/release/runsys
```

## 使用说明

### 准备 OCI Bundle

OCI Bundle 是一个包含以下内容的目录：
- `config.json`：OCI 运行时配置文件
- `rootfs/`：容器的根文件系统

项目包含一个测试用的 BusyBox Bundle：

```bash
# 查看测试 Bundle 结构
ls -la busybox-test/
# 输出: config.json rootfs/
```

### 命令行接口

#### 创建容器

```bash
# 用法: runsys create <container-id> <bundle-path>
sudo ./target/release/runsys create mycontainer ./busybox-test

# 输出示例:
# [runsys]: 尝试从 bundle 创建容器 'mycontainer': "./busybox-test"
# [runsys]: 运行时目录已准备: /run/runsys/mycontainer
# [runsys]: 正在读取 OCI 配置: ./busybox-test/config.json
# [runsys]: 容器 'mycontainer' 状态已持久化 (state: Created)
# Container created successfully!
```

#### 启动容器

```bash
# 用法: runsys start <container-id>
sudo ./target/release/runsys start mycontainer

# 容器将以前台模式运行，提供交互式终端
```

### 容器状态流转

```
Creating → Created → Running → Stopped
   ↑         ↑        ↓
   └────────┴────────┘
```

- **Creating**：正在初始化容器
- **Created**：容器已创建，等待启动
- **Running**：容器正在运行
- **Stopped**：容器已停止

## 技术实现细节

### 1. 容器生命周期管理

容器状态通过 `state.json` 文件持久化保存：

```json
{
  "oci_version": "1.0.2",
  "id": "mycontainer",
  "status": "created",
  "pid": null,
  "bundle": "/path/to/bundle"
}
```

### 2. 命名空间隔离

使用 `unshare` 系统调用创建以下命名空间：

| 命名空间 | 标志 | 作用 |
|---------|------|------|
| Mount | `CLONE_NEWNS` | 挂载点隔离 |
| UTS | `CLONE_NEWUTS` | 主机名/域名隔离 |
| IPC | `CLONE_NEWIPC` | 进程间通信隔离 |
| Network | `CLONE_NEWNET` | 网络栈隔离 |
| PID | `CLONE_NEWPID` | 进程号隔离 |

### 3. 根文件系统隔离 (pivot_root)

1. 将 `rootfs` 目录 bind mount 到自己，使其成为挂载点
2. 使用 `pivot_root` 切换根目录
3. 卸载旧的根文件系统 (`/.old_root`)
4. 在新命名空间中重新挂载 `/proc` 和 `/sys`

### 4. Cgroup V2 资源限制

支持以下资源限制：

| 资源 | Cgroup V2 文件 | 说明 |
|------|---------------|------|
| 内存限制 | `memory.max` | 最大可用内存 |
| 内存预留 | `memory.low` | 内存预留值 |
| CPU 限制 | `cpu.max` | CPU 时间配额 |
| CPU 权重 | `cpu.weight` | CPU 调度权重 |
| PIDs 限制 | `pids.max` | 最大进程数 |

### 5. 进程架构

```
宿主机
  ├── 父进程 (Parent)
  │   ├── 等待子进程退出
  │   ├── PTY Master 数据中继
  │   └── 应用 Cgroup 限制
  └── 子进程 (Child - 中间进程)
      └── 子进程 (PID 1 - 容器主进程)
          ├── 设置控制终端
          ├── 挂载 /proc, /sys
          └── execve 执行目标程序
```

### 6. 终端处理

- 使用 PTY (伪终端) 实现宿主机与容器的交互
- 父进程将宿主机终端切换至 Raw Mode，透传所有字符
- 通过双线程实现输入/输出的双向中继

## 配置示例 (config.json)

```json
{
  "ociVersion": "1.2.1",
  "process": {
    "terminal": true,
    "user": { "uid": 0, "gid": 0 },
    "args": ["/bin/sh"],
    "env": [
      "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
      "TERM=xterm"
    ],
    "cwd": "/"
  },
  "root": {
    "path": "rootfs",
    "readonly": true
  },
  "hostname": "runc",
  "linux": {
    "resources": {
      "memory": { "limit": 268435456 },
      "cpu": { "quota": 100000, "period": 100000 }
    }
  }
}
```

## 错误处理

项目定义了完善的错误类型 (`RuntimeError`)：

- `InvalidAction`：在当前状态下执行不允许的操作
- `InvalidBundle`：OCI Bundle 路径无效
- `ConfigNotFound`：找不到 `config.json`
- `ConfigParseError`：OCI 配置解析失败
- `ContainerNotFound`：容器不存在
- `ContainerAlreadyExists`：容器已存在
- `NixError`：Linux 系统调用错误
- `CgroupError`：Cgroup 操作错误

## 开发计划

- [x] 容器创建与启动
- [x] OCI 状态管理
- [x] 命名空间隔离
- [x] 根文件系统切换 (pivot_root)
- [x] Cgroup V2 资源限制
- [x] PTY 终端支持
- [ ] 更多生命周期操作 (Kill, Delete, State 查询)
- [ ] 挂载配置支持
- [ ] 钩子 (Hooks) 支持
- [ ] 命名空间路径配置

## 相关资源

- [OCI Runtime Spec](https://github.com/opencontainers/runtime-spec)
- [runC](https://github.com/opencontainers/runc) - OCI 参考实现
- [Nix Rust 文档](https://docs.rs/nix/latest/nix/)

## 许可证

MIT License

## 作者

Mzh - 一个用于学习和实践 Linux 容器技术的 Rust 项目
