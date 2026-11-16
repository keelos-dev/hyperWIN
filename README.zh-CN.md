# hyperWIN

**Windows 上的虚拟机，3D 加速不用折腾，自带 RDP 服务端把画面送出来。** 底下是微软自己的 hypervisor。

[下载](https://github.com/keelos-dev/hyperWIN/releases) · [English](README.md) · Mac 上的同门：[hyperMAC](https://github.com/keelos-dev/hyperMAC)

Windows 早就有显卡半虚拟化（GPU-PV）：虚拟机直接用宿主的显卡，不需要第二块卡，也不用直通。但要自己用 PowerShell 加显卡、手工把宿主的显卡驱动文件拷进虚拟机、宿主驱动一升级就得重拷，最后还得想办法流畅地看到虚拟机的画面。这些 hyperWIN 都自己做了。

> **早期版本（0.1）。** 目前只在一种机器上用过：Windows 10 专业版 22H2、Intel UHD 770、Windows 10 虚拟机。别的环境可能有毛病，欢迎告诉我们。

## 功能

- **虚拟机走 Host Compute System**，就是 WSL 2 和 Windows 沙盒底下那一层。只需要 Windows 的「虚拟机平台」功能，不用装 Hyper-V。
- **3D 加速，并且一直能用。** 勾了 GPU 的虚拟机用宿主的显卡；每次启动前 hyperWIN 自动把虚拟机里的驱动文件和宿主当前的驱动对齐。
- **Windows 自己装好。** 给它微软的原版 ISO、一个用户名和密码，大约十分钟后虚拟机就到桌面了，带 3D。
- **看得舒服的画面。** 一个 RDP 端口服务所有虚拟机。用 [kRDPclient](https://github.com/keelos-dev/kRDPclient-releases) 连，画面是宿主显卡硬编的 H.264，最高 60 帧；Windows 自带的 `mstsc` 也能连（NLA，位图）。安装包自带 kRDPclient：窗口程序里的「控制台」按钮用它在本机直接打开虚拟机的画面。
- **声音。** Hyper-V 不给虚拟机声卡。hyperWIN 模拟一块 Intel HD Audio，Windows 虚拟机用自带驱动就认；声音送到客户端（Opus 或 PCM）。
- **剪贴板：文字、图片、文件和文件夹**，虚拟机和客户端之间双向。虚拟机里有个小程序负责，hyperWIN 自己把它放进去。
- **电脑重启，虚拟机自己回来。** Windows 关机或重启时，运行中的虚拟机存到磁盘，开机后接着跑（不带 GPU 和声卡的：Linux、FreeBSD）；其余的请来宾正常关机，开机后再启动。
- **虚拟机里不用手动装任何东西**：没有工具光盘，没有代理要配。
- **有窗口程序，也有命令行。** `hyperwin-app.exe` 有「主机」页（谁连着、暂停、各个开关）和「虚拟机」页；`hyperwin.exe` 在终端里做同样的事，并作为 Windows 服务运行。

## 上手

1. 从 [Releases](https://github.com/keelos-dev/hyperWIN/releases) 下载 `hyperwin-...-setup.exe`，双击。它先检查这台电脑（Windows 版本、处理器虚拟化、「虚拟机平台」功能、显卡、内存、磁盘、端口），有挡路的就告诉你怎么处理，没有就安装。开着 Hyper-V 的电脑上它和 Hyper-V 并存，Hyper-V 自己的虚拟机不动。
2. 不用安装器也行：下载 zip 并解压，在**管理员**终端里，进到那个文件夹：

   ```powershell
   .\hyperwin.exe install          # 打开「虚拟机平台」，装上服务和窗口程序
   ```

   如果 Windows 需要重启才能启用这个功能，它会说；重启后再运行一次。
3. 从开始菜单打开 **hyperWIN**。在「主机」页设好账号密码；在「虚拟机」页点「新建...」，选微软的 Windows 10 ISO，勾上「自动安装 Windows」，填虚拟机里的用户名和密码，启动。
4. 连接：在 kRDPclient 里把这台电脑添加为主机（地址、账号）；或者用 `mstsc` 连 `这台电脑:3390`，用户名写 `账号@虚拟机名`。

命令行里做同样的事：

```powershell
hyperwin passwd admin
hyperwin vm add w10 --disk D:\vm\w10.vhdx --disk-gb 64 --iso D:\iso\Win10.iso `
                    --cpus 4 --memory 8192 --gpu --unattend ben:secret
hyperwin vm start w10
```

全部命令在 [crates/hyperwin/src/main.rs](crates/hyperwin/src/main.rs) 开头。

## 需要什么

- Windows 10 22H2 或 Windows 11，64 位，固件里开了虚拟化。家庭版应该可以（只需要「虚拟机平台」），但没有试过。
- 3D 需要显卡驱动支持 GPU-PV（现在的驱动大多支持）。试过的：Intel UHD 770。
- 管理员权限：在 Windows 上虚拟机是管理员的事。

## 已知的限制

- **还不支持 Windows 11 虚拟机**：它要 TPM，而只开「虚拟机平台」时没有虚拟 TPM。
- 试过的虚拟机系统：Windows 10。Linux 和 FreeBSD 能启动、能看到控制台，没有 3D、声音和剪贴板。
- 宿主是 Windows 10 时，让虚拟机关机是通过它的键盘（HCS 从 Windows 11 起才有正规的办法）：虚拟机的控制台上得有人登录着。
- 程序没有代码签名：第一次运行时 SmartScreen 会提示。
- 没有快照，没有 USB 直通，所有虚拟机共用一个 NAT 网络。
- 窗口程序的语言：英文、中文、日文。

## 编译

Rust（stable，MSVC）、带 CMake 的 Visual Studio 生成工具、Git：

```sh
sh tools/vendor-ironrdp.sh                                   # 取 IronRDP 并打补丁，放进 vendor/
cargo build --release -p hyperwin -p hyperwin-app
cargo build --release --manifest-path guest/Cargo.toml       # 放进虚拟机的小程序
```

把 `hyperwin.exe`、`hyperwin-app.exe`、`hyperwin-guest.exe` 放在同一个文件夹里，运行 `hyperwin.exe install`。更多见 [docs/DEVELOPING.md](docs/DEVELOPING.md)。

## 许可证

MIT（`LICENSE`）。第三方代码及其许可证：[THIRD_PARTY.md](THIRD_PARTY.md)。
