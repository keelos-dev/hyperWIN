//! The app's words in English, Chinese and Japanese: the one of Windows' display language.

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
    TABLE.iter().find(|(e, _, _)| *e == en).map_or(en, |(_, zh, ja)| if l == Lang::Zh { zh } else { ja })
}

const TABLE: &[(&str, &str, &str)] = &[
    ("New...", "新建...", "新規..."),
    (
        "Hyper-V's compute service (vmcompute) has not answered for {} s: the list shows what it said last.",
        "Hyper-V 的计算服务（vmcompute）已 {} 秒没有回应：列表显示的是它上次的回答。",
        "Hyper-V のコンピューティング サービス (vmcompute) が {} 秒応答していません。一覧は前回の応答のままです。",
    ),
    ("Start", "启动", "起動"),
    ("Power Off", "断电", "電源オフ"),
    ("Shut Down", "关机", "シャットダウン"),
    ("shutting down...", "正在关机...", "シャットダウン中..."),
    ("Eject CD", "取出光盘", "CD を取り出す"),
    ("Remove", "删除", "削除"),
    ("Password...", "密码...", "パスワード..."),
    ("Settings...", "设置...", "設定..."),
    ("Settings of {}", "{} 的设置", "{} の設定"),
    ("CD: an ISO file (empty: none)", "光盘：ISO 文件（留空则没有）", "CD: ISO ファイル (空欄: なし)"),
    ("Install Service", "安装服务", "サービスをインストール"),
    ("Start Service", "启动服务", "サービスを開始"),
    ("Service: not installed", "服务：未安装", "サービス: 未インストール"),
    ("New Token", "新令牌", "新しいトークン"),
    ("Name", "名称", "名前"),
    ("State", "状态", "状態"),
    ("System", "系统", "システム"),
    ("CPUs", "CPU", "CPU"),
    ("Memory", "内存", "メモリ"),
    ("Screen", "屏幕", "画面"),
    ("GPU", "GPU", "GPU"),
    ("CD", "光盘", "CD"),
    ("running", "运行中", "実行中"),
    ("stopped", "已停止", "停止"),
    ("starting...", "正在启动...", "起動中..."),
    ("powering off...", "正在断电...", "電源オフ中..."),
    ("creating...", "正在创建...", "作成中..."),
    ("yes", "有", "あり"),
    ("in", "有", "あり"),
    ("Open hyperWIN", "打开 hyperWIN", "hyperWIN を開く"),
    ("Exit", "退出", "終了"),
    ("OK", "确定", "OK"),
    ("Cancel", "取消", "キャンセル"),
    ("New Virtual Machine", "新建虚拟机", "新しい仮想マシン"),
    ("Windows", "Windows", "Windows"),
    ("Linux", "Linux", "Linux"),
    ("Install ISO (optional)", "安装 ISO（可不填）", "インストール ISO (任意)"),
    ("Disk file (.vhdx; empty: in the machine's own folder)", "磁盘文件 (.vhdx；留空：放在这台虚拟机自己的文件夹里)", "ディスク ファイル (.vhdx。空欄: この仮想マシンのフォルダー内)"),
    ("Disk size (GB), when the file is new", "磁盘大小 (GB)，新建文件时", "ディスク サイズ (GB)、新規ファイルのとき"),
    ("Memory (MB)", "内存 (MB)", "メモリ (MB)"),
    ("3D acceleration (the PC's GPU)", "3D 加速（使用本机显卡）", "3D アクセラレーション (この PC の GPU)"),
    ("Start with hyperWIN", "随 hyperWIN 启动", "hyperWIN と一緒に起動"),
    (
        "Install Windows by itself (needs the ISO)",
        "自动安装 Windows（需要 ISO）",
        "Windows を自動インストール (ISO が必要)",
    ),
    ("Windows account", "Windows 账号", "Windows アカウント"),
    ("Its password", "该账号的密码", "そのパスワード"),
    ("Start it now", "立即启动", "すぐに起動"),
    ("A name is needed.", "需要填写名称。", "名前が必要です。"),
    (
        "An unattended install needs the ISO, an account and its password.",
        "自动安装需要 ISO、账号和密码。",
        "自動インストールには ISO、アカウント、パスワードが必要です。",
    ),
    ("hyperWIN's Account", "hyperWIN 账号", "hyperWIN のアカウント"),
    ("User", "用户名", "ユーザー"),
    ("Password", "密码", "パスワード"),
    ("Password again", "再输一次密码", "パスワード (確認)"),
    (
        "The account kRDPclient (add this PC as a host) and mstsc log in with.",
        "kRDPclient（把这台电脑添加为主机）和 mstsc 登录时使用的账号。",
        "kRDPclient (この PC をホストとして追加) と mstsc がログインに使うアカウントです。",
    ),
    ("The two passwords differ.", "两次输入的密码不一致。", "2 つのパスワードが一致しません。"),
    ("A user name and a password are needed.", "需要用户名和密码。", "ユーザー名とパスワードが必要です。"),
    ("The account is set.", "账号已设置。", "アカウントを設定しました。"),
    (
        "A new token, shown only now (it is on the clipboard too):",
        "新令牌，只显示这一次（已复制到剪贴板）：",
        "新しいトークンです。表示は今だけです (クリップボードにもコピーしました):",
    ),
    ("Remove the virtual machine {}? Its disk file stays.", "删除虚拟机 {}？磁盘文件会保留。", "仮想マシン {} を削除しますか? ディスク ファイルは残ります。"),
    ("Power off {}? What it has not saved is lost.", "给 {} 断电？未保存的内容会丢失。", "{} の電源を切りますか? 保存していない内容は失われます。"),
    ("Service: running", "服务：运行中", "サービス: 実行中"),
    ("Service: stopped", "服务：已停止", "サービス: 停止"),
    ("kRDPclient: add the host {}", "kRDPclient：添加主机 {}", "kRDPclient: ホスト {} を追加"),
    ("Account: {}", "账号：{}", "アカウント: {}"),
    ("Account: none yet (Password...)", "账号：尚未设置（点“密码...”）", "アカウント: 未設定 (「パスワード...」)"),
    ("hyperWIN keeps running in the notification area.", "hyperWIN 仍在通知区域运行。", "hyperWIN は通知領域で動き続けます。"),
    ("Disk files", "磁盘文件", "ディスク ファイル"),
    ("ISO images", "ISO 映像", "ISO イメージ"),
    ("All files", "所有文件", "すべてのファイル"),
    ("Host", "主机", "ホスト"),
    ("Virtual Machines", "虚拟机", "仮想マシン"),
    ("Copy Address", "复制地址", "アドレスをコピー"),
    ("Console", "控制台", "コンソール"),
    ("User name: {}", "用户名：{}", "ユーザー名: {}"),
    ("SERVICE", "服务", "サービス"),
    ("REMOTE", "远程", "リモート"),
    ("CONNECTION", "连接", "接続"),
    ("SETTINGS", "设置", "設定"),
    ("ACCOUNT", "账号", "アカウント"),
    ("STORAGE", "存储位置", "保存場所"),
    ("Change...", "更改...", "変更..."),
    ("{} GB free", "可用 {} GB", "空き {} GB"),
    ("This folder is not there now (is the disk attached?)", "这个位置现在不在（磁盘没接上？）", "このフォルダーは現在ありません (ディスクは接続されていますか?)"),
    (
        "Stop the running virtual machines first: after the change the list shows only the machines of the new place.",
        "请先停止运行中的虚拟机：更改后，列表只显示新位置里的虚拟机。",
        "先に実行中の仮想マシンを停止してください。変更後、一覧には新しい場所の仮想マシンだけが表示されます。",
    ),
    ("Choose a folder for the virtual machines and the installers", "选一个文件夹放虚拟机和安装盘", "仮想マシンとインストーラーを置くフォルダーを選んでください"),
    (
        "It holds guests (one folder per virtual machine) and iso (installers). The machines there are were not moved: to take one along, move its folder into the new place's guests.",
        "里面是 guests（每台虚拟机一个文件夹）和 iso（安装盘）。已有的虚拟机没有搬动：要带过去的话，把它的文件夹移到新位置的 guests 里。",
        "中には guests (仮想マシンごとに 1 フォルダー) と iso (インストーラー) があります。既存の仮想マシンは移動していません。持っていくには、そのフォルダーを新しい場所の guests に移してください。",
    ),
    ("Certificate fingerprint (SHA-256)", "证书指纹（SHA-256）", "証明書のフィンガープリント (SHA-256)"),
    ("kRDPclient token (the account's password works too)", "kRDPclient 令牌（直接用账号密码也行）", "kRDPclient トークン (アカウントのパスワードでも可)"),
    ("They apply from the next connection on.", "从下一次连接起生效。", "次の接続から有効になります。"),
    ("Disconnect", "断开", "切断"),
    ("Copy", "复制", "コピー"),
    ("Get kRDPclient", "下载 kRDPclient", "kRDPclient を入手"),
    ("Uninstall...", "卸载...", "アンインストール..."),
    ("Restart Service", "重启服务", "サービスを再起動"),
    ("Pause remote: no new connections; the ones up stay", "暂停远程：不接受新连接，已连着的不断开", "リモートを一時停止: 新しい接続を受け付けません (接続中のものはそのまま)"),
    ("H.264 hardware video (for kRDPclient; off: plain bitmaps)", "H.264 硬件视频（kRDPclient 用；关掉则走位图）", "H.264 ハードウェア ビデオ (kRDPclient 用。オフ: ビットマップ)"),
    ("Sound: the virtual machines' sound on the client", "声音：把虚拟机的声音传到客户端", "サウンド: 仮想マシンの音をクライアントで再生"),
    ("Clipboard: copy and paste text, pictures and files both ways", "剪贴板：文字、图片、文件双向复制粘贴", "クリップボード: テキスト、画像、ファイルを双方向でコピー・貼り付け"),
    ("Keep this PC from sleeping while a virtual machine is running", "有虚拟机在运行时不让这台电脑睡眠", "仮想マシンの実行中はこの PC をスリープさせない"),
    ("Not installed", "未安装", "未インストール"),
    ("Service not running", "服务没在运行", "サービスは実行されていません"),
    ("Remote paused: no new connections", "已暂停远程：不接受新连接", "リモートを一時停止中: 新しい接続を受け付けません"),
    ("Service running", "服务运行中", "サービス実行中"),
    ("Nobody connected", "没有连接", "接続なし"),
    ("user: account@VM", "用户名：账号@虚拟机名", "ユーザー: アカウント@VM"),
    ("hyperWIN: the service is not running", "hyperWIN：服务没在运行", "hyperWIN: サービスは実行されていません"),
    ("hyperWIN: paused, no new connections", "hyperWIN：已暂停，不接受新连接", "hyperWIN: 一時停止中 (新しい接続を受け付けません)"),
    ("hyperWIN: waiting for connections", "hyperWIN：等待连接", "hyperWIN: 接続待ち"),
    ("hyperWIN: {} connected", "hyperWIN：{} 个连接", "hyperWIN: {} 件接続中"),
    ("Resume Remote", "恢复远程", "リモートを再開"),
    ("Pause Remote", "暂停远程", "リモートを一時停止"),
    ("Sound card", "声卡", "サウンド カード"),
    ("Clipboard (hyperWIN puts a small helper into the guest)", "剪贴板（hyperWIN 会往来宾里放一个小程序）", "クリップボード (hyperWIN がゲストに小さなヘルパーを入れます)"),
    ("Stop hyperWIN and remove its service? The virtual machines and their disks stay; running ones keep running, without clients.", "停止 hyperWIN 并移除它的服务？虚拟机和磁盘都会保留；正在运行的会继续运行，只是客户端连不上。", "hyperWIN を停止してサービスを削除しますか? 仮想マシンとディスクは残ります。実行中のものは動き続けますが、クライアントは接続できません。"),
];

/// `t(en)` with its `{}` replaced.
pub fn tf(en: &'static str, value: &str) -> String {
    t(en).replacen("{}", value, 1)
}
