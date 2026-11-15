//! A VM's console, with nothing installed in the guest.
//!
//! HCS gives a VM's picture on a named pipe (`Devices.VideoMonitor.ConnectionOptions.NamedPipe`
//! in its configuration). Behind that pipe the VM worker process is an RDP server that wants no
//! login. This crate is the client of it:
//!
//! - the RDP connection itself (IronRDP, through [`hyperwin_stdsec`] because that server only
//!   speaks Standard RDP Security with RC4) carries keyboard and mouse;
//! - the picture does not come as RDP graphics: on the dynamic channel
//!   `Microsoft::Windows::RDS::Frame_Buffer::Control::v08.01` the server, having recognised a
//!   client on the same machine, hands over shared memory with the raw 32-bit frame
//!   (`ironrdp_vmconnect::FrameBufferClient`).
//!
//! What this needs from the machine (docs/design.md, section 10): the "VMMS certificate" in
//! the machine's certificate store, or the pipe answers nothing; the Terminal Server
//! `InstanceID` in the registry, which is the proof of "same machine".
//!
//! Measured on the dev box: about 40 frames a second at 1280x800 whatever changes on screen.

#![cfg(windows)]

use anyhow::{anyhow, Context as _};
use ironrdp::connector::{self, ClientConnector, ConnectorResult, Credentials};
use ironrdp::dvc::DrdynvcClient;
use ironrdp::pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::input::MousePdu;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, NetworkClient, TokioFramed};
use ironrdp_vmconnect::FrameBufferClient;
use std::time::Duration;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Something for the guest's keyboard or mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A key by its PC scancode (set 1). `extended`: the E0-prefixed keys (arrows, right Ctrl...).
    Key { scancode: u8, extended: bool, down: bool },
    /// The pointer, in pixels of the VM's screen.
    MouseMove { x: u16, y: u16 },
    MouseButton { button: Button, down: bool, x: u16, y: u16 },
    /// Wheel steps: positive is away from the user; 120 is one notch.
    Wheel { delta: i16, x: u16, y: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// An open console. Dropping it ends the connection.
pub struct Console {
    commands: mpsc::UnboundedSender<Command>,
    task: JoinHandle<anyhow::Result<()>>,
}

enum Command {
    Input(Input),
    Refresh,
}

impl Console {
    /// Queues input for the guest. Fails when the connection is gone.
    pub fn send(&self, input: Input) -> anyhow::Result<()> {
        self.commands.send(Command::Input(input)).map_err(|_| anyhow!("the console connection has ended"))
    }

    /// Asks the VM's server to send the whole screen again (an RDP Refresh Rect).
    ///
    /// The frame-buffer channel delivers a frame when the screen changes; on a screen that
    /// stands still a console that has just been opened may wait for its first frame
    /// (seen on the dev box: nothing for 8 s). This is how to ask for one.
    pub fn refresh(&self) -> anyhow::Result<()> {
        self.commands.send(Command::Refresh).map_err(|_| anyhow!("the console connection has ended"))
    }

    /// Waits for the connection to end and says why.
    pub async fn closed(mut self) -> anyhow::Result<()> {
        // the input sender stays alive meanwhile: the session must not end for lack of one
        (&mut self.task).await.unwrap_or_else(|e| Err(anyhow!("console task: {e}")))
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How long one read may take while the connection is being set up. The server pauses for
/// seconds after the Client Info PDU (its connect-time auto-detect).
const SETUP_STEP: Duration = Duration::from_secs(30);

/// Opens the console of the VM whose video pipe is `\\.\pipe\<pipe>`.
///
/// `width` x `height` should be the size of the VM's configured monitor: asked for another,
/// the server deactivates and activates anew with its own, and the frames have that size (as
/// they have while the guest runs without a display driver of its own).
///
/// `on_frame` is called for every frame the server hands over: the whole picture, row by row,
/// one `0x00RRGGBB` per pixel. A running guest's frames come through the frame-buffer channel
/// (from a thread of its own). Before the guest has a display driver of its own (the firmware,
/// a boot loader's prompt, Windows Setup's first stage) the server sends ordinary RDP bitmap
/// updates instead: those are drawn here and handed over the same way.
pub async fn open<F>(pipe: &str, width: u16, height: u16, on_frame: F) -> anyhow::Result<Console>
where
    F: Fn(Vec<u32>, u16, u16) + Send + Sync + 'static,
{
    let path = format!(r"\\.\pipe\{pipe}");
    let on_frame = std::sync::Arc::new(on_frame);
    let pipe = ClientOptions::new().open(&path).with_context(|| format!("open {path}"))?;
    let mut framed = TokioFramed::new(hyperwin_stdsec::wrap(pipe));

    let instance_id = ironrdp_vmconnect::local_instance_id().map_err(|e| anyhow!("the Terminal Server instance id (the proof of a same-machine client): {e}"))?;
    // The address is only reported to the server in the Client Info PDU.
    let client_addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    let mut connector = ClientConnector::new(config(width, height, instance_id), client_addr)
        // drdynvc carries the frame-buffer channel. A static channel is also what keeps this
        // server's RC4 stream in step (see hyperwin-stdsec).
        .with_static_channel(DrdynvcClient::new().with_dynamic_channel(FrameBufferClient::new({
            let on_frame = on_frame.clone();
            move |pixels, width, height| on_frame(pixels, width, height)
        })));

    let result = tokio::time::timeout(SETUP_STEP * 3, async {
        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector).await?;
        // Standard RDP Security: there is no TLS to upgrade to.
        let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
        ironrdp_tokio::connect_finalize(upgraded, connector, &mut framed, &mut NoNetwork, "hyperwin".to_owned().into(), Vec::new(), None)
            .await
    })
    .await
    .map_err(|_| anyhow!("the VM's console did not finish connecting in {} s", SETUP_STEP.as_secs() * 3))?
    .map_err(|e| anyhow!("connect to the VM's console: {e:#}"))?;
    tracing::info!(width = result.desktop_size.width, height = result.desktop_size.height, "console connected");

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let ended = session(framed, result, rx, on_frame).await;
        if let Err(e) = &ended {
            tracing::info!("console ended: {e:#}");
        }
        ended
    });
    Ok(Console { commands: tx, task })
}

/// The client's side of the connection: what it tells the server about itself.
fn config(width: u16, height: u16, instance_id: String) -> connector::Config {
    connector::Config {
        // The VM's console asks for no login.
        credentials: Credentials::UsernamePassword { username: String::new(), password: String::new() },
        domain: None,
        enable_tls: false,
        enable_credssp: false,
        enable_standard_rdp_security: true,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        // the same-machine proof for the frame-buffer channel
        dig_product_id: instance_id,
        desktop_size: connector::DesktopSize { width, height },
        monitor_layout: None,
        bitmap: None,
        client_build: 0,
        client_name: "hyperwin".to_owned(),
        client_dir: r"C:\Windows\System32\mstscax.dll".to_owned(),
        platform: MajorPlatformType::WINDOWS,
        // the picture comes through the frame-buffer channel, pointer included
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp::pdu::rdp::capability_sets::RailSupportLevel::empty(),
    }
}

/// CredSSP's helper for reaching a KDC; never used on this connection.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(&mut self, _request: &ironrdp::connector::sspi::generator::NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(connector::ConnectorError::new("network request on a pipe connection", connector::ConnectorErrorKind::General))
    }
}

type Stream = tokio::io::DuplexStream;

async fn session<F>(mut framed: TokioFramed<Stream>, result: connector::ConnectionResult, mut commands: mpsc::UnboundedReceiver<Command>, on_frame: std::sync::Arc<F>) -> anyhow::Result<()>
where
    F: Fn(Vec<u32>, u16, u16) + Send + Sync + 'static,
{
    let (mut width, mut height) = (result.desktop_size.width, result.desktop_size.height);
    // for the server's changes of size (below)
    let activation_factory = result.activation_factory;
    // What RDP graphics are drawn into: the picture while the guest has no display driver of
    // its own. Once it has, the picture is in shared memory and nothing is drawn here.
    let mut image = DecodedImage::new(ironrdp::graphics::image_processing::PixelFormat::RgbA32, result.desktop_size.width, result.desktop_size.height);
    let mut stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();

    loop {
        let outputs = tokio::select! {
            pdu = framed.read_pdu() => {
                let (action, payload) = pdu.context("read from the VM's console")?;
                stage.process(&mut image, action, &payload).map_err(|e| anyhow!("console session: {e:#}"))?
            }
            command = commands.recv() => {
                let Some(first) = command else { return Ok(()) };
                // whatever else is waiting goes in the same PDU
                let (mut events, mut refresh) = (Vec::new(), false);
                let mut next = Some(first);
                while let Some(command) = next {
                    match command {
                        Command::Input(input) => events.push(fast_path(input)),
                        Command::Refresh => refresh = true,
                    }
                    next = commands.try_recv().ok();
                }
                let mut outputs = stage.process_fastpath_input(&mut image, &events).map_err(|e| anyhow!("console input: {e:#}"))?;
                if refresh {
                    // Refresh Rect, not the Suppress Output toggle: the plain request first
                    let frames = stage.request_full_redraw(width, height, true, false).map_err(|e| anyhow!("console refresh: {e:#}"))?;
                    outputs.extend(frames.into_iter().map(ActiveStageOutput::ResponseFrame));
                }
                outputs
            }
        };
        let mut drawn = false;
        for out in outputs {
            match out {
                ActiveStageOutput::GraphicsUpdate(_) => drawn = true,
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await.context("write to the VM's console")?,
                ActiveStageOutput::Terminate(reason) => return Err(anyhow!("the VM's console ended the session: {reason:?}")),
                // The screen changed its size: the server deactivates and activates anew with
                // the new one (MS-RDPBCGR 1.3.1.3). A VM does that when its guest's display
                // driver starts or stops: the firmware and Windows Setup's first stage have a
                // size of their own (1024x768), not the monitor's configured one.
                ActiveStageOutput::DeactivateAll => {
                    let mut activation = activation_factory.create();
                    let mut buf = ironrdp::core::WriteBuf::new();
                    loop {
                        ironrdp_tokio::single_sequence_step(&mut framed, &mut activation, &mut buf).await.map_err(|e| anyhow!("the console's change of size: {e:#}"))?;
                        if let ironrdp::connector::connection_activation::ConnectionActivationState::Finalized {
                            desktop_size,
                            share_id,
                            enable_server_pointer,
                            pointer_software_rendering,
                            static_channel_chunk_size,
                            ..
                        } = activation.connection_activation_state()
                        {
                            (width, height) = (desktop_size.width, desktop_size.height);
                            image = DecodedImage::new(ironrdp::graphics::image_processing::PixelFormat::RgbA32, width, height);
                            if !stage.reactivate(activation.io_channel_id(), activation.user_channel_id(), share_id, enable_server_pointer, pointer_software_rendering, static_channel_chunk_size) {
                                return Err(anyhow!("the console's change of size: a channel chunk size the session cannot take"));
                            }
                            tracing::info!(width, height, "the console's screen changed its size");
                            break;
                        }
                    }
                    // what else came with the old activation is of no use now
                    break;
                }
                _ => {}
            }
        }
        if drawn {
            // R, G, B, A bytes -> 0x00RRGGBB, the whole picture once per batch of updates
            let pixels: Vec<u32> = image.data().chunks_exact(4).map(|p| u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2])).collect();
            on_frame(pixels, width, height);
        }
    }
}

fn fast_path(input: Input) -> FastPathInputEvent {
    let mouse = |flags: PointerFlags, wheel: i16, x: u16, y: u16| {
        FastPathInputEvent::MouseEvent(MousePdu { flags, number_of_wheel_rotation_units: wheel, x_position: x, y_position: y })
    };
    match input {
        Input::Key { scancode, extended, down } => {
            let mut flags = KeyboardFlags::empty();
            flags.set(KeyboardFlags::RELEASE, !down);
            flags.set(KeyboardFlags::EXTENDED, extended);
            FastPathInputEvent::KeyboardEvent(flags, scancode)
        }
        Input::MouseMove { x, y } => mouse(PointerFlags::MOVE, 0, x, y),
        Input::MouseButton { button, down, x, y } => {
            let mut flags = match button {
                Button::Left => PointerFlags::LEFT_BUTTON,
                Button::Right => PointerFlags::RIGHT_BUTTON,
                Button::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
            };
            flags.set(PointerFlags::DOWN, down);
            mouse(flags, 0, x, y)
        }
        Input::Wheel { delta, x, y } => mouse(PointerFlags::VERTICAL_WHEEL, delta, x, y),
    }
}
