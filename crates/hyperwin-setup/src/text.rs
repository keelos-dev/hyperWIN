//! The installer's words in English, Chinese and Japanese: the one of Windows' display language.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    En,
    Zh,
    Ja,
}

fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| {
        // SAFETY: no arguments, no state.
        let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        match id & 0x3ff {
            0x04 => Lang::Zh,
            0x11 => Lang::Ja,
            _ => Lang::En,
        }
    })
}

/// `en` in the display language (itself when the table has no such line).
pub fn t(en: &'static str) -> &'static str {
    let l = lang();
    if l == Lang::En {
        return en;
    }
    TABLE
        .iter()
        .find(|(e, _, _)| *e == en)
        .map_or(en, |(_, zh, ja)| if l == Lang::Zh { zh } else { ja })
}

/// `t(en)` with each `{}` replaced in turn.
pub fn tf(en: &'static str, values: &[&str]) -> String {
    values
        .iter()
        .fold(t(en).to_owned(), |s, v| s.replacen("{}", v, 1))
}

const TABLE: &[(&str, &str, &str)] = &[
    ("hyperWIN Setup", "hyperWIN 安装", "hyperWIN セットアップ"),
    ("Item", "项目", "項目"),
    ("Result", "结果", "結果"),
    ("Checking this PC...", "正在检查这台电脑...", "この PC を確認しています..."),
    ("Check Again", "重新检查", "再確認"),
    ("Install", "安装", "インストール"),
    ("Update", "更新", "更新"),
    ("Close", "关闭", "閉じる"),
    ("Restart Windows", "重启 Windows", "Windows を再起動"),
    ("Open hyperWIN", "打开 hyperWIN", "hyperWIN を開く"),
    ("Windows", "Windows", "Windows"),
    ("Processor", "处理器", "プロセッサ"),
    ("Virtual Machine Platform", "虚拟机平台", "仮想マシン プラットフォーム"),
    ("Hyper-V", "Hyper-V", "Hyper-V"),
    ("Graphics", "显卡", "グラフィックス"),
    ("Memory", "内存", "メモリ"),
    ("Disk", "磁盘", "ディスク"),
    ("Network ports", "网络端口", "ネットワーク ポート"),
    ("hyperWIN", "hyperWIN", "hyperWIN"),
    ("{} (build {}), 64-bit", "{}（内部版本 {}），64 位", "{} (ビルド {})、64 ビット"),
    (
        "{} (build {}): hyperWIN needs 64-bit Windows 10 version 2004 or later, or Windows 11",
        "{}（内部版本 {}）：hyperWIN 需要 64 位的 Windows 10 2004 及以上版本，或 Windows 11",
        "{} (ビルド {}): hyperWIN には 64 ビットの Windows 10 バージョン 2004 以降、または Windows 11 が必要です",
    ),
    ("Virtualization is on", "虚拟化已开启", "仮想化は有効です"),
    (
        "Virtualization is off: turn on Intel VT-x (or AMD-V / SVM) in the PC's firmware (BIOS) setup, then check again",
        "虚拟化没有开启：请在电脑的固件（BIOS）设置里打开 Intel VT-x（或 AMD-V / SVM），然后重新检查",
        "仮想化が無効です: PC のファームウェア (BIOS) 設定で Intel VT-x (または AMD-V / SVM) を有効にしてから、再確認してください",
    ),
    (
        "This processor cannot run Windows' hypervisor (no second-level address translation)",
        "这个处理器跑不了 Windows 的 hypervisor（没有二级地址转换）",
        "このプロセッサでは Windows のハイパーバイザーを実行できません (第 2 レベル アドレス変換がありません)",
    ),
    ("On", "已开启", "有効"),
    (
        "Off: setup turns it on. Windows then has to restart; run setup again afterwards",
        "未开启：安装时会打开它。之后 Windows 需要重启，重启后请再运行一次安装程序",
        "無効: セットアップで有効にします。その後 Windows の再起動が必要です。再起動後にもう一度セットアップを実行してください",
    ),
    (
        "Turned on, waiting for a restart of Windows: restart, then run setup again",
        "已打开，等待重启 Windows：请重启后再运行一次安装程序",
        "有効化済み、Windows の再起動待ち: 再起動してから、もう一度セットアップを実行してください",
    ),
    ("Not on (hyperWIN does not need it)", "未开启（hyperWIN 不需要它）", "無効 (hyperWIN には不要です)"),
    (
        "On, with {} virtual machine(s) of its own. hyperWIN uses the same hypervisor and runs beside it; Hyper-V's machines stay where they are",
        "已开启，它自己有 {} 台虚拟机。hyperWIN 用的是同一个 hypervisor，和它并存；Hyper-V 的虚拟机原样不动",
        "有効、Hyper-V 自身の仮想マシンが {} 台あります。hyperWIN は同じハイパーバイザーを使い、並行して動きます。Hyper-V の仮想マシンはそのままです",
    ),
    ("{}: no 3D for the virtual machines (Windows' basic display driver)", "{}：虚拟机没有 3D（Windows 的基本显示驱动）", "{}: 仮想マシンで 3D は使えません (Windows の基本ディスプレイ ドライバー)"),
    ("{} GB", "{} GB", "{} GB"),
    ("{} GB: little for virtual machines (8 GB or more is better)", "{} GB：跑虚拟机偏少（建议 8 GB 以上）", "{} GB: 仮想マシンには少なめです (8 GB 以上を推奨)"),
    ("{} GB free on {}", "{} GB 可用（{}）", "{} GB の空き ({})"),
    ("{} GB free on {}: a Windows guest alone takes 30 GB or more", "只剩 {} GB（{}）：一台 Windows 虚拟机就要 30 GB 以上", "{} GB の空き ({}): Windows ゲスト 1 台で 30 GB 以上使います"),
    ("RDP 3390, kRDPclient {}", "RDP 3390，kRDPclient {}", "RDP 3390、kRDPclient {}"),
    (
        "RDP 3390; 443 is taken by {}, so kRDPclient gets {}: add this PC there as ADDRESS:{}",
        "RDP 3390；443 被 {} 占用，kRDPclient 改用 {}：在 kRDPclient 里添加主机时写「地址:{}」",
        "RDP 3390。443 は {} が使用中のため、kRDPclient には {} を使います: kRDPclient では「アドレス:{}」で追加してください",
    ),
    (
        "Port 3390 is taken by {}: hyperWIN needs it. Stop that program, then check again",
        "端口 3390 被 {} 占用：hyperWIN 需要这个端口。请先停掉那个程序，再重新检查",
        "ポート 3390 は {} が使用中です: hyperWIN に必要です。そのプログラムを停止してから、再確認してください",
    ),
    ("Not installed yet", "尚未安装", "未インストール"),
    ("Installed: setup replaces it with this version; the virtual machines stay", "已安装：安装程序会换成这个版本，虚拟机保留", "インストール済み: このバージョンに置き換えます。仮想マシンは残ります"),
    ("This PC is ready for hyperWIN {}.", "这台电脑可以安装 hyperWIN {}。", "この PC に hyperWIN {} をインストールできます。"),
    ("hyperWIN cannot be installed until the items marked X are dealt with.", "标了 X 的项目处理好之前，装不了 hyperWIN。", "X の付いた項目を解決するまで hyperWIN をインストールできません。"),
    ("Installing...", "正在安装...", "インストール中..."),
    ("hyperWIN is installed. Open it from the Start menu.", "hyperWIN 已安装。可以从开始菜单打开。", "hyperWIN をインストールしました。スタート メニューから開けます。"),
    (
        "Windows has to restart to finish turning on the Virtual Machine Platform. Restart, then run this setup again.",
        "Windows 需要重启才能完成「虚拟机平台」的启用。重启后请再运行一次这个安装程序。",
        "仮想マシン プラットフォームの有効化を完了するには Windows の再起動が必要です。再起動後、このセットアップをもう一度実行してください。",
    ),
    ("The installation did not complete:", "安装没有完成：", "インストールが完了しませんでした:"),
    ("Restart Windows now? Save your work first.", "现在重启 Windows 吗？请先保存好手头的工作。", "今すぐ Windows を再起動しますか? 先に作業を保存してください。"),
    ("The checks could not be run:", "检查没能运行：", "確認を実行できませんでした:"),
];
