# hyperWIN（开发说明）

Windows 上的虚拟机软件：用微软自己的 hypervisor，把 Hyper-V 已有但要自己折腾的 3D 加速（GPU-PV）做成点一下就有，画面用内置的 H.264 RDP 服务端送到 kRDPclient。hyperMAC 在 Windows 上的对应物。

这份是给开发的人看的。开发在私有仓库 `hyperWIN-dev` 里进行；公开仓库 `hyperWIN` 每个版本收一份快照（`tools/publish.sh`），在那里这份文件是 `docs/DEVELOPING.md`，介绍在 `README.md`。下面标了「只在开发仓库」的东西，公开仓库里没有。

## 结构

- `crates/`：正式代码，一个 Cargo 工作区。
  - `hyperwin`：命令行和 Windows 服务（`hyperwin.exe`）。全部命令在 `crates/hyperwin/src/main.rs` 开头。
  - `hyperwin-app`：宿主上的窗口程序和通知区域图标（`hyperwin-app.exe`，原生 Win32）。
  - `hyperwin-vm`：虚拟机是什么（`vm.json`）、启停、显卡驱动同步、无人值守安装、每台虚拟机的宿主进程。
  - `hyperwin-hcs`：Host Compute System、Host Compute Network 和设备虚拟化（HDV）的绑定。
  - `hyperwin-hda`：模拟的 Intel HD Audio 声卡。
  - `hyperwin-console`、`hyperwin-stdsec`：连虚拟机的画面管道。
  - `hyperwin-h264`：Media Foundation 的 H.264 编码。
  - `hyperwin-rdp`：对外的 RDP 服务端、给 kRDPclient 的网页接口、局域网发现、声音、剪贴板。
  - `hyperwin-guest`：来宾里的小程序和宿主之间的连接（Hyper-V 套接字、分帧）。
- `guest/`：放进 Windows 来宾的小程序（`hyperwin-guest.exe`，剪贴板）。单独的一个工程，不是工作区成员：它建在另一份 IronRDP 上。
- `tools/`：`vendor-ironrdp.sh`（取 IronRDP 并打补丁）、`third-party.py`（第三方声明）、`build-release.ps1` 和 `package.ps1`（出发布包）、`publish.sh`（导出到公开仓库）、`test/`（没有图形界面时用的测试工具）。
- 只在开发仓库：`docs/design.md`（设计和验证结论）、`docs/notes/`（真机验证的记录）、`spikes/`（最初的试验程序）、`tools/devbox/`（测试机的工具）、`AGENTS.md`。

## 编译

要 Rust（stable，MSVC 工具链）、Visual Studio 生成工具（C++、Windows SDK、CMake：Opus 的源码用它编）、Git。

```sh
sh tools/vendor-ironrdp.sh                                   # 两份 IronRDP 放进 vendor/
cargo build --release -p hyperwin -p hyperwin-app
cargo build --release --manifest-path guest/Cargo.toml       # 产物在 guest/target/release/
cargo test --workspace --exclude hyperwin-app                # app 的测试程序要管理员权限才起得来
```

cmake 不在 PATH 上时，加上 Visual Studio 自带的那份（`...\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`）。

试用：把 `hyperwin.exe`、`hyperwin-app.exe`、`hyperwin-guest.exe` 放在一个文件夹里，在管理员终端运行 `hyperwin.exe install`（它把自己拷到 `%ProgramFiles%\hyperWIN` 并重启服务）。旁边再放一个 `krdpclient.exe` 的话，app 的「控制台」按钮用它（H.264），否则用 mstsc。

## 出一个版本

1. 改 `Cargo.toml` 和 `guest/Cargo.toml` 里的版本号。`Cargo.lock` 变过就重新生成第三方声明：`python tools/third-party.py`（要 keelOS 仓库里的许可证检查工具）。提交。
2. `powershell -File tools\build-release.ps1`：从不带本机用户名的路径编译（程序里会留下源码路径）。
   同时编出单文件安装器 `target\dist\release\hyperwin-setup.exe`（`crates/hyperwin-setup`：双击后先检查这台电脑，再安装；三个程序和许可证都带在里面）。自己用的可以加 `-With <krdpclient.exe>` 把 kRDPclient 也带上（控制台按钮用）；带了在本机原地编的程序就不能发布，下一步会拦住。
3. `powershell -File tools\package.ps1`：检查并打成 `dist\hyperwin-<主.次>-<提交>-<时间>-windows-x64.zip`，旁边放一份 `...-windows-x64-setup.exe`，打印 SHA-256。
4. `sh tools/publish.sh ../hyperWIN`：把这次提交的内容作为一个提交放进公开仓库的本地克隆。看过之后自己 `git push`。
5. 在公开仓库建 release：tag 是 zip 的文件名去掉 `.zip`，附件是 zip 和安装器，正文写这一版的说明和 SHA-256。

## 许可证

MIT。第三方代码见 `THIRD_PARTY.md`。
