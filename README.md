# hyperWIN

**Virtual machines on Windows with 3D acceleration that just works, and a built-in RDP server
to reach them.** Microsoft's own hypervisor underneath, no Hyper-V Manager in sight.

[Download](https://github.com/keelos-dev/hyperWIN/releases) · [中文](README.zh-CN.md) ·
its sibling on the Mac: [hyperMAC](https://github.com/keelos-dev/hyperMAC)

Windows has had GPU paravirtualization (GPU-PV) for years: a guest uses the host's graphics card,
no second card, no passthrough. But it is left to the user to add the adapter with PowerShell,
copy the host's display driver files into the guest by hand, copy them again after every
driver update, and then find a way to see the guest's screen at more than a slideshow.
hyperWIN does all of that by itself.

> **An early release (0.1).** It has been used on one kind of PC so far: Windows 10 Pro 22H2,
> an Intel UHD 770, Windows 10 guests. Expect rough edges elsewhere, and tell us what you find.

## What it does

- **Virtual machines through the Host Compute System**, the interface under WSL 2 and Windows
  Sandbox. It needs only Windows' "Virtual Machine Platform" feature, not the Hyper-V role.
- **3D acceleration, kept working.** A VM with `GPU` gets the host's GPU (GPU-PV), and before
  every start hyperWIN brings the driver files inside the guest in step with the host's
  current driver.
- **Windows installs itself.** Give it Microsoft's ISO, a user name and a password: about ten
  minutes later the VM is at its desktop, with 3D.
- **A screen you can use.** One RDP port serves every VM's console. With
  [kRDPclient](https://github.com/keelos-dev/kRDPclient-releases) the picture is H.264 from
  the host's hardware encoder at up to 60 frames a second; Windows' own `mstsc` connects too
  (NLA, bitmaps).
- **Sound.** Hyper-V gives a VM no sound card. hyperWIN emulates one (Intel HD Audio), which
  Windows guests take with their own driver; the sound goes to the client (Opus or PCM).
- **Clipboard: text, pictures, files and folders**, both ways, between the guest and the
  client. A small helper does it inside the guest; hyperWIN puts it there itself.
- **Nothing to install in the guest** by hand: no tools ISO, no agent setup.
- **An app and a command line.** The app (`hyperwin-app.exe`) has a Host page (who is
  connected, pause, the switches) and a Virtual Machines page; `hyperwin.exe` does the same
  from a terminal and runs as a Windows service.

## Getting started

1. Download the zip from [Releases](https://github.com/keelos-dev/hyperWIN/releases) and unpack it.
2. In an **elevated** terminal, in that folder:

   ```powershell
   .\hyperwin.exe install          # turns on "Virtual Machine Platform", installs the service and the app
   ```

   It says so if Windows has to restart for the feature; run it again afterwards.
3. Open **hyperWIN** from the Start menu. On the Host page set the account's password; on the
   Virtual Machines page click **New...**, pick Microsoft's Windows 10 ISO, tick "Install
   Windows by itself", give the guest's user name and password, and start it.
4. Connect: in kRDPclient add this PC as a host (its address, the account); or `mstsc` to
   `this-PC:3390` with the user name `account@VM-name`.

The same from a terminal:

```powershell
hyperwin passwd admin
hyperwin vm add w10 --disk D:\vm\w10.vhdx --disk-gb 64 --iso D:\iso\Win10.iso `
                    --cpus 4 --memory 8192 --gpu --unattend ben:secret
hyperwin vm start w10
```

All commands are at the top of [crates/hyperwin/src/main.rs](crates/hyperwin/src/main.rs).

## What it needs

- Windows 10 22H2 or Windows 11, 64-bit, with virtualization on in the firmware. Home should
  do (only "Virtual Machine Platform" is needed) but has not been tried.
- For 3D, a GPU whose driver supports GPU-PV (most current ones do). Tried: Intel UHD 770.
- Administrator rights: virtual machines are an administrator's business on Windows.

## Known limits

- **Windows 11 guests are not supported yet**: they need a TPM, and the virtual TPM is not
  there with "Virtual Machine Platform" alone.
- Guests tried: Windows 10. Linux and FreeBSD guests boot and show their console, without 3D,
  sound or clipboard.
- A guest is asked to shut down through its keyboard on a Windows 10 host (HCS has the proper
  way only from Windows 11 on): somebody must be signed in at its console.
- The programs are not code-signed: SmartScreen warns at the first start.
- No snapshots, no USB passthrough, one NAT network for all VMs.
- The app's languages: English, Chinese, Japanese.

## Building

Rust (stable, MSVC), the Visual Studio build tools with CMake, Git:

```sh
sh tools/vendor-ironrdp.sh                                   # IronRDP with the patches, into vendor/
cargo build --release -p hyperwin -p hyperwin-app
cargo build --release --manifest-path guest/Cargo.toml       # the helper for the guests
```

Put `hyperwin.exe`, `hyperwin-app.exe` and `hyperwin-guest.exe` in one folder and run
`hyperwin.exe install`. More in [docs/DEVELOPING.md](docs/DEVELOPING.md) (Chinese).

## License

MIT (`LICENSE`). Third-party code and its licenses: [THIRD_PARTY.md](THIRD_PARTY.md).
