use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::Result;
use clap::Parser;
use nayutacast::{
    AVFrameStreamPlayer, AVFrameStreamPlayerOptions, AudioOptions, Capture, DiscoveryContext,
    DiscoveryObserver, DiscoveryService, NayutacastReceiver, NayutacastReceiverOptions,
    NayutacastSender, NayutacastSenderMediaOptions, NayutacastSenderOptions,
    NayutacastSenderTrackOptions, MediaStreamDescription, Size, SourceType, TransportOptions,
    TransportStrategy, VideoDecoderType, VideoEncoderType, VideoOptions,
    VideoRenderOptionsBuilder, VideoRenderSurfaceOptions, create_receiver, create_sender,
    get_runtime_handle, shutdown, startup,
};

use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

trait GetSize {
    fn size(&self) -> Size;
}

impl GetSize for Window {
    fn size(&self) -> Size {
        let size = self.inner_size();
        Size {
            width: size.width,
            height: size.height,
        }
    }
}

#[derive(Debug)]
enum Events {
    CreateReceiver(Ipv4Addr, MediaStreamDescription),
    StartSender,
    StartReceiver,
    Stop,
}

struct Observer {
    events: Arc<EventLoopProxy<Events>>,
    description: Option<MediaStreamDescription>,
}

impl DiscoveryObserver for Observer {
    async fn online(&self, mut ctx: DiscoveryContext<'_>) {
        if let Some(description) = &self.description {
            let _ = ctx.broadcast(serde_json::to_vec(description).unwrap());
        }
    }

    async fn on_message(&self, ctx: DiscoveryContext<'_>, message: Vec<u8>) -> () {
        if let Ok(message) = serde_json::from_slice(&message) {
            self.events
                .send_event(Events::CreateReceiver(ctx.ip, message))
                .unwrap();
        }
    }
}

#[allow(unused)]
struct Sender {
    sender: NayutacastSender<AVFrameStreamPlayer<'static>, ()>,
    discovery: DiscoveryService,
}

impl Sender {
    fn new(
        event_loop: Arc<EventLoopProxy<Events>>,
        window: Arc<Window>,
        configure: &Configure,
    ) -> Result<Self> {
        let video_options = configure.get_video_options();

        // Get the first screen that can be captured.
        let mut video = None;
        if let Some(source) = Capture::get_sources(SourceType::Screen)?
            .iter()
            .find(|it| it.is_default)
        {
            video = Some(NayutacastSenderTrackOptions {
                options: video_options.clone(),
                source: source.clone(),
            });
        }

        // Get the first audio input device that can be captured.
        let mut audio = None;
        if let Some(source) = Capture::get_sources(SourceType::Audio)?
            .iter()
            .find(|it| it.is_default)
        {
            audio = Some(NayutacastSenderTrackOptions {
                source: source.clone(),
                options: AudioOptions {
                    sample_rate: 48000,
                    bit_rate: 64000,
                },
            });
        }

        let options = NayutacastSenderOptions {
            media: NayutacastSenderMediaOptions { video, audio },
            transport: TransportOptions {
                strategy: configure.get_strategy().unwrap(),
                mtu: 1500,
            },
        };

        let sender = create_sender(
            &options,
            AVFrameStreamPlayer::new(AVFrameStreamPlayerOptions::OnlyVideo(
                VideoRenderOptionsBuilder::new(VideoRenderSurfaceOptions {
                    size: window.size(),
                    window,
                })
                .from_sender(&options)
                .build(),
            ))?,
            (),
        )?;

        // Register the current sender's information with the LAN discovery service so
        // that other receivers can know that the sender has been created and can access
        // the sender's information.
        let discovery = get_runtime_handle().block_on(DiscoveryService::new(
            "nayutacast-example".to_string(),
            Observer {
                description: Some(sender.get_description().clone()),
                events: event_loop,
            },
        ))?;

        Ok(Self { discovery, sender })
    }
}

#[allow(unused)]
struct Receiver(NayutacastReceiver<AVFrameStreamPlayer<'static>, ()>);

impl Receiver {
    fn new(
        configure: Configure,
        window: Arc<Window>,
        ip: Ipv4Addr,
        mut description: MediaStreamDescription,
    ) -> Result<Self> {
        let video_decoder = configure.decoder;

        // The sender, if using passthrough, will need to replace the ip in the publish
        // address by replacing the ip address with the sender's ip.
        if let TransportStrategy::Direct(addr) = &mut description.transport.strategy {
            addr.set_ip(IpAddr::V4(ip));
        }

        let options = NayutacastReceiverOptions { video_decoder };
        let receiver = create_receiver(
            &description,
            &options,
            AVFrameStreamPlayer::new(AVFrameStreamPlayerOptions::All(
                VideoRenderOptionsBuilder::new(VideoRenderSurfaceOptions {
                    size: window.size(),
                    window,
                })
                .from_receiver(&description, &options)
                .build(),
            ))?,
            (),
        )?;

        Ok(Self(receiver))
    }
}

struct App {
    event_loop: Arc<EventLoopProxy<Events>>,
    service: Option<DiscoveryService>,
    window: Option<Arc<Window>>,
    receiver: Option<Receiver>,
    sender: Option<Sender>,
}

impl App {
    fn new(event_loop: Arc<EventLoopProxy<Events>>) -> Self {
        Self {
            receiver: None,
            service: None,
            sender: None,
            window: None,
            event_loop,
        }
    }

    fn start_sender(&mut self) {
        if self.sender.is_none() {
            if let Some(window) = self.window.clone() {
                println!("[+] Starting SENDER (Screen capture & streaming)...");
                match Sender::new(self.event_loop.clone(), window, &Configure::parse()) {
                    Ok(sender) => {
                        println!("[✓] SENDER started successfully!");
                        self.sender.replace(sender);
                    }
                    Err(e) => {
                        eprintln!("[!] Failed to start SENDER: {:?}", e);
                    }
                }
            } else {
                eprintln!("[!] Window not ready yet.");
            }
        } else {
            println!("[*] SENDER is already running.");
        }
    }

    fn start_receiver(&mut self) {
        if self.service.is_none() {
            println!("[+] Starting RECEIVER discovery service...");
            match get_runtime_handle().block_on(DiscoveryService::new(
                "nayutacast-example".to_string(),
                Observer {
                    events: self.event_loop.clone(),
                    description: None,
                },
            )) {
                Ok(service) => {
                    println!("[✓] Discovery active. Waiting for Senders on network...");
                    self.service.replace(service);
                }
                Err(e) => {
                    eprintln!("[!] Failed to start Discovery Service: {:?}", e);
                }
            }
        } else {
            println!("[*] RECEIVER discovery is already active.");
        }
    }

    fn stop(&mut self) {
        println!("[-] Stopping SENDER and RECEIVER...");
        drop(self.receiver.take());
        drop(self.sender.take());
        drop(self.service.take());
        println!("[✓] Stopped.");
    }
}

impl ApplicationHandler<Events> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attr = Window::default_attributes();
        attr.title = "nayutacast example".to_string();
        attr.active = true;
        attr.inner_size = Some(winit::dpi::Size::Physical(PhysicalSize::new(1280, 720)));

        let window = Arc::new(event_loop.create_window(attr).unwrap());
        self.window.replace(window);

        startup().unwrap();

        let config = Configure::parse();
        println!();
        println!("============================================================");
        println!("  Nayutacast Example (Window & Terminal Controls Ready)    ");
        println!("============================================================");
        println!("  • Press 's' + Enter (or press 'S' in GUI) -> SENDER");
        println!("  • Press 'r' + Enter (or press 'R' in GUI) -> RECEIVER");
        println!("  • Press 'k' + Enter (or press 'K' in GUI) -> STOP");
        println!("============================================================");
        println!();

        if let Some(mode) = &config.mode {
            match mode.to_lowercase().as_str() {
                "s" | "sender" => {
                    let _ = self.event_loop.send_event(Events::StartSender);
                }
                "r" | "receiver" => {
                    let _ = self.event_loop.send_event(Events::StartReceiver);
                }
                _ => {
                    eprintln!("[!] Unknown mode: '{}'. Expected 'sender' or 'receiver'.", mode);
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.stop();
                event_loop.exit();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !event.repeat && event.state == ElementState::Released {
                    if let PhysicalKey::Code(key) = event.physical_key {
                        match key {
                            KeyCode::KeyS => {
                                self.start_sender();
                            }
                            KeyCode::KeyR => {
                                self.start_receiver();
                            }
                            KeyCode::KeyK => {
                                self.stop();
                            }
                            _ => (),
                        }
                    }
                }
            }
            _ => (),
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: Events) {
        match event {
            Events::CreateReceiver(ip, description) => {
                if let (None, Some(window)) = (&self.receiver, &self.window) {
                    println!("[+] Connected to Sender at {}, rendering stream...", ip);
                    self.receiver.replace(
                        Receiver::new(Configure::parse(), window.clone(), ip, description).unwrap(),
                    );
                }
            }
            Events::StartSender => {
                self.start_sender();
            }
            Events::StartReceiver => {
                self.start_receiver();
            }
            Events::Stop => {
                self.stop();
            }
        }
    }
}

#[derive(Parser)]
#[command(
    about = env!("CARGO_PKG_DESCRIPTION"),
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
)]
struct Configure {
    /// The address to which the nayutacast service is bound, indicating how to
    /// connect to the nayutacast service.
    #[arg(long)]
    address: Option<SocketAddr>,
    /// direct, relay, multicast
    #[arg(long)]
    strategy: Option<String>,
    /// Auto-start mode: 'sender' or 'receiver'
    #[arg(long)]
    mode: Option<String>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long, default_value_t = 24)]
    fps: u8,
    /// Each sender and receiver need to be bound to a channel, and the receiver
    /// can only receive the cast screen within the channel.
    #[arg(long, default_value_t = 0)]
    id: u32,
    #[arg(
        long,
        value_parser = clap::value_parser!(VideoEncoderType),
        default_value_t = Self::DEFAULT_ENCODER,
    )]
    encoder: VideoEncoderType,
    #[arg(
        long,
        value_parser = clap::value_parser!(VideoDecoderType),
        default_value_t = Self::DEFAULT_DECODER,
    )]
    decoder: VideoDecoderType,
}

impl Configure {
    #[cfg(target_os = "macos")]
    const DEFAULT_ENCODER: VideoEncoderType = VideoEncoderType::VideoToolBox;

    #[cfg(target_os = "windows")]
    const DEFAULT_ENCODER: VideoEncoderType = VideoEncoderType::Qsv;

    #[cfg(target_os = "linux")]
    const DEFAULT_ENCODER: VideoEncoderType = VideoEncoderType::X264;

    #[cfg(target_os = "macos")]
    const DEFAULT_DECODER: VideoDecoderType = VideoDecoderType::VideoToolBox;

    #[cfg(target_os = "windows")]
    const DEFAULT_DECODER: VideoDecoderType = VideoDecoderType::D3D11;

    #[cfg(target_os = "linux")]
    const DEFAULT_DECODER: VideoDecoderType = VideoDecoderType::H264;

    fn get_strategy(&self) -> Option<TransportStrategy> {
        Some(match self.strategy.as_ref()?.as_str() {
            "direct" => TransportStrategy::Direct(self.address?),
            "relay" => TransportStrategy::Relay(self.address?),
            "multicast" => TransportStrategy::Multicast(self.address?),
            _ => unreachable!(),
        })
    }

    fn get_video_options(&self) -> VideoOptions {
        VideoOptions {
            codec: self.encoder,
            frame_rate: self.fps,
            width: self.width,
            height: self.height,
            bit_rate: 10000000,
            key_frame_interval: 21,
        }
    }
}

fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Info)?;

    Configure::parse();

    // Creates a message loop, which is used to create the main window.
    let event_loop = EventLoop::<Events>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = Arc::new(event_loop.create_proxy());

    let proxy_stdin = proxy.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(line) = line {
                let cmd = line.trim().to_lowercase();
                match cmd.as_str() {
                    "s" | "sender" => {
                        let _ = proxy_stdin.send_event(Events::StartSender);
                    }
                    "r" | "receiver" => {
                        let _ = proxy_stdin.send_event(Events::StartReceiver);
                    }
                    "k" | "stop" | "q" | "quit" => {
                        let _ = proxy_stdin.send_event(Events::Stop);
                    }
                    "" => (),
                    _ => {
                        println!("Unknown command: '{}'. Type 's' (Sender), 'r' (Receiver), or 'k' (Stop).", cmd);
                    }
                }
            }
        }
    });

    event_loop.run_app(&mut App::new(proxy))?;

    // When exiting the application, the environment of nayutacast should be cleaned
    // up.
    shutdown()?;
    Ok(())
}
