use anyhow::Result;
use clap::{App, AppSettings, Arg};
use interceptor::registry::Registry;
use media::io::h264_writer::H264Writer;
use media::io::ogg_writer::OggWriter;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::media::rtp::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::media::rtp::rtp_receiver::RTCRtpReceiver;
use webrtc::media::track::track_remote::TrackRemote;
use webrtc::peer::configuration::RTCConfiguration;
use webrtc::peer::ice::ice_connection_state::RTCIceConnectionState;
use webrtc::peer::ice::ice_server::RTCIceServer;
use webrtc::peer::sdp::session_description::RTCSessionDescription;

async fn save_to_disk(
    writer: Arc<Mutex<dyn media::io::Writer + Send + Sync>>,
    track: Arc<TrackRemote>,
    notify: Arc<Notify>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = track.read_rtp() => {
                if let Ok((rtp_packet, _)) = result {
                    let mut w = writer.lock().await;
                    w.write_rtp(&rtp_packet)?;
                }else{
                    println!("file closing begin after read_rtp error");
                    let mut w = writer.lock().await;
                    if let Err(err) = w.close() {
                        println!("file close err: {}", err);
                    }
                    println!("file closing end after read_rtp error");
                    return Ok(());
                }
            }
            _ = notify.notified() => {
                println!("file closing begin after notified");
                let mut w = writer.lock().await;
                if let Err(err) = w.close() {
                    println!("file close err: {}", err);
                }
                println!("file closing end after notified");
                return Ok(());
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut app = App::new("save-to-disk-h264")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs>")
        .about("An example of save-to-disk-h264.")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::with_name("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::with_name("debug")
                .long("debug")
                .short("d")
                .help("Prints debug log information"),
        )
        .arg(
            Arg::with_name("video")
                .required_unless("FULLHELP")
                .takes_value(true)
                .short("v")
                .long("video")
                .help("Video file to be streaming."),
        )
        .arg(
            Arg::with_name("audio")
                .required_unless("FULLHELP")
                .takes_value(true)
                .short("a")
                .long("audio")
                .help("Audio file to be streaming."),
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let debug = matches.is_present("debug");
    if debug {
        env_logger::Builder::new()
            .format(|buf, record| {
                writeln!(
                    buf,
                    "{}:{} [{}] {} - {}",
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.level(),
                    chrono::Local::now().format("%H:%M:%S.%6f"),
                    record.args()
                )
            })
            .filter(None, log::LevelFilter::Trace)
            .init();
    }

    let video_file = matches.value_of("video").unwrap();
    let audio_file = matches.value_of("audio").unwrap();

    let h264_writer: Arc<Mutex<dyn media::io::Writer + Send + Sync>> =
        Arc::new(Mutex::new(H264Writer::new(File::create(video_file)?)));
    let ogg_writer: Arc<Mutex<dyn media::io::Writer + Send + Sync>> = Arc::new(Mutex::new(
        OggWriter::new(File::create(audio_file)?, 48000, 2)?,
    ));

    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    // Setup the codecs you want to use.
    // We'll use a H264 and Opus but you can also define your own
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;

    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Allow us to receive 1 audio track, and 1 video track
    peer_connection
        .add_transceiver_from_kind(RTPCodecType::Audio, &[])
        .await?;
    peer_connection
        .add_transceiver_from_kind(RTPCodecType::Video, &[])
        .await?;

    let notify_tx = Arc::new(Notify::new());
    let notify_rx = notify_tx.clone();

    // Set a handler for when a new remote track starts, this handler saves buffers to disk as
    // an ivf file, since we could have multiple video tracks we provide a counter.
    // In your application this is where you would handle/process video
    let pc = Arc::clone(&peer_connection);
    peer_connection.on_track(Box::new(move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
        if let Some(track) = track {
            // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
            let media_ssrc = track.ssrc();
            let pc2 = Arc::clone(&pc);
            tokio::spawn(async move {
                let mut result = Result::<usize>::Ok(0);
                while result.is_ok() {
                    let timeout = tokio::time::sleep(Duration::from_secs(3));
                    tokio::pin!(timeout);

                    tokio::select! {
                        _ = timeout.as_mut() =>{
                            result = pc2.write_rtcp(&PictureLossIndication{
                                    sender_ssrc: 0,
                                    media_ssrc,
                            }).await.map_err(Into::into);
                        }
                    };
                }
            });

            let notify_rx2 = Arc::clone(&notify_rx);
            let h264_writer2 = Arc::clone(&h264_writer);
            let ogg_writer2 = Arc::clone(&ogg_writer);
            Box::pin(async move {
                let codec = track.codec().await;
                let mime_type = codec.capability.mime_type.to_lowercase();
                if mime_type == MIME_TYPE_OPUS.to_lowercase() {
                    println!("Got Opus track, saving to disk as output.opus (48 kHz, 2 channels)");     
                    tokio::spawn(async move {
                        let _ = save_to_disk(ogg_writer2, track, notify_rx2).await;
                    });
                } else if mime_type == MIME_TYPE_H264.to_lowercase() {
                    println!("Got h264 track, saving to disk as output.h264");
                     tokio::spawn(async move {
                         let _ = save_to_disk(h264_writer2, track, notify_rx2).await;
                     });
                }
            })
        }else {
            Box::pin(async {})
        }
	})).await;

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    let pc = Arc::clone(&peer_connection);
    peer_connection
        .on_ice_connection_state_change(Box::new(move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {}", connection_state);

            let pc2 = Arc::clone(&pc);
            let notify_tx2 = Arc::clone(&notify_tx);
            Box::pin(async move {
                if connection_state == RTCIceConnectionState::Connected {
                    println!("Ctrl+C the remote client to stop the demo");
                } else if connection_state == RTCIceConnectionState::Failed {
                    notify_tx2.notify_waiters();

                    println!("Done writing media files");

                    // Gracefully shutdown the peer connection
                    if let Err(err) = pc2.close().await {
                        println!("peer_connection close err: {}", err);
                    }

                    std::process::exit(0);
                }
            })
        }))
        .await;

    // Wait for the offer to be pasted
    let line = signal::must_read_stdin()?;
    let desc_data = signal::decode(line.as_str())?;
    let offer = serde_json::from_str::<RTCSessionDescription>(&desc_data)?;

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = signal::encode(&json_str);
        println!("{}", b64);
    } else {
        println!("generate local_description failed!");
    }

    println!("Press ctrl-c to stop");
    tokio::signal::ctrl_c().await.unwrap();

    Ok(())
}
