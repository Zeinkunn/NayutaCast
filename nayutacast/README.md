A development toolkit for writing low-latency and cross-platform screen casting applications.

Nayutacast is a cross-platform screen casting SDK for writing screen casting applications using the Rust programming language. It provides a very high level of abstraction, shields cross-platform details, codec support for software and hardware, and supports multiple transmission methods. This project prioritizes latency and performance. Currently, the end-to-end latency is controlled below 250ms across supported platforms.

-   Transmission method: SRT transmission protocol, UDP multicast.
-   Codec support: Open H264, X264, Intel Qsv, Video Toolbox.

## Quick Start

nayutacast does not need to specifically address cross-platform differences and feature flags:

```toml
nayutacast = "0.4"
```

### Creating a Sender

The sender is also the initiator of the screen casting, before initiating the screen casting, you need to determine the capture source, here we capture the screen and the system sound as an example.

Get the list of screens:

```rust
let video_sources = Capture::get_sources(SourceType::Screen)?;
```

`sources` is a list of all the screens on your current device, where `is_default` with `true` is your default monitor.

Get the system sound output device:

```rust
let audio_sources = Capture::get_sources(SourceType::Audio)?;
```

The audio output device follows the same rules as the monitor above.

To keep things a bit simpler, we use the first source of the audio source list and the video source list.

```rust
let video_source = video_sources.get(0)?;
let audio_source = audio_sources.get(0)?;
```

Next, create the sender configurations.

Start by creating the encoding configurations for the audio and video sources:

```rust
let video_options = NayutacastSenderTrackOptions {
    source: video_source,
    options: VideoOptions {
        codec: VideoEncoderType::X264,
        frame_rate: 30,
        width: 1280,
        height: 720,
        bit_rate: 500 * 1024 * 8,
        key_frame_interval: 21,
    },
};

let audio_options = NayutacastSenderTrackOptions {
    source: audio_source,
    options: AudioOptions {
        sample_rate: 48000,
        bit_rate: 64000,
    },
};
```

The video encoder uses a software encoder and is fixed at 30 frames per second for `1280x720` video. Audio is fixed at `48khz` sample rate.

Then, create the sender.

First, we use UDP multicast as the network transmission method for screen casting:

```rust
let transport = TransportOptions {
    strategy: TransportStrategy::Multicast("239.0.0.1:8080".parse()?),
    mtu: 1500,
};
```

The sender will send the audio and video packets to port `8080` on `239.0.0.1`.

Pass these configurations to the `create_sender` function to create the sender:

```rust
let sender = nayutacast::create_sender(
    &NayutacastSenderOptions {
        transport,
        media: NayutacastSenderMediaOptions {
            video: Some(video_options),
            audio: Some(audio_options),
        },
    },
    view,
    observer,
)?;
```

You may notice that `view` we haven't created yet, don't worry, we'll go back and create `view` to display our sender preview screen next.

```rust
struct View(Mutex<VideoRender<'a>>);

impl AVFrameStream for View {}

impl MediaStreamSink for View {
    fn audio(&self, frame: &AudioFrame) -> bool {
        true
    }

    fn video(&self, frame: &VideoFrame) -> bool {
        self.video.send(frame).is_ok()
    }
}

impl AVFrameObserver for View {
    fn close(&self) {
        println!("view is closed");
    }
}
```

We implement `AVFrameStream` for `View`, which is needed for `create_sender`. We submit every video frame we receive to the `VideoRender`, but the audio we don't process because playing native sound will cause an audio loopback (you don't want that).

Let's go back to how to create the `VideoRender`.

The renderer needs a window to output and display the screen, it is recommended to use `winit` to create a native window:

```rust
let inner_size = window.inner_size();
let video_render = VideoRender::new(VideoRenderOptions {
    backend: VideoRenderBackend::WebGPU,
    target: window,
    size: Size {
        width: inner_size.width,
        height: inner_size.height,
    },
})?;
```

Then create `View`.

```rust
let view = View(video_render);
```

This way, we have a window where we can preview the video screen.

After the creation of the sender is complete, it is important to note that you need to get the unique identifier of the sender via `sender.get_id()`, which is needed by the receiver to find the sender.

### Creating a Receiver

Receiver is used to receive audio and video streams from the sender. The relationship between a receiver and a sender is not one-to-one, and multiple receivers can receive audio and video streams from a sender at the same time.

Creating a receiver is much simpler:

```rust
let receiver = nayutacast::create_receiver(
    &description,
    &NayutacastReceiverOptions {
        video_decoder: VideoDecoderType::H264,
    },
    view,
    observer,
)?;
```

The `description` comes from the sender, for video decoding we use a software decoder, and the transport layer policy needs to be the same on the receiver side as on the sender side.

## LAN discovery

Considering that if there is no mechanism for LAN discovery, the creation process between the sender and the receiver requires an external server to intervene and synchronize some signaling and configuration information. Nayutacast has a built-in LAN discovery component, where you can register a service with `DiscoveryService` and pass its description, so that other devices can query the registered service for information.

The `DiscoveryService` provides interfaces for registration and querying.

```rust
let service = DiscoveryService::new("nayutacast-service".to_string(), observer).await?;
```
