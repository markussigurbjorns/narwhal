use anyhow::Result;
use bytes::Bytes;
use gstreamer::prelude::*;
use gstreamer::{self as gst};
use gstreamer_app as gst_app;
use std::sync::Arc;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::warn;

const RTP_TAP_BUFFER: usize = 256;
const RTP_INJECTOR_BUFFER: usize = 256;
//const RTP_TAP_BUFFER: usize = 10;
//const RTP_INJECTOR_BUFFER: usize = 10;

#[derive(Clone, Debug)]
pub struct RtpPacket {
    pub pad_name: String,
    pub caps: gst::Caps,
    pub data: Bytes,
    pub meta: RtpMeta,
    pub pts: Option<gst::ClockTime>,
    pub dts: Option<gst::ClockTime>,
    pub duration: Option<gst::ClockTime>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtpMeta {
    pub ssrc: Option<u32>,
    pub sequence_number: Option<u16>,
    pub timestamp: Option<u32>,
    pub temporal_layer: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct RtpStreamInfo {
    pub media_key: String,
    pub caps: gst::Caps,
}

pub struct RtpTap {
    pub rx: mpsc::Receiver<RtpPacket>,
}

pub struct RtpInjector {
    pub tx: mpsc::Sender<RtpPacket>,
}

impl RtpInjector {
    pub fn sender(&self) -> mpsc::Sender<RtpPacket> {
        self.tx.clone()
    }
}

/// Install an RTP tap: whenever webrtcbin adds a src_%u pad (application/x-rtp)
/// we link it to an appsink and forward buffers to a tokio channel.
pub fn install_rtp_tap(pipeline: &gst::Pipeline, webrtcbin: &gst::Element) -> Result<RtpTap> {
    let (tx, rx) = mpsc::channel::<RtpPacket>(RTP_TAP_BUFFER);
    let pipeline_weak = pipeline.downgrade();
    let tx = Arc::new(tx);

    webrtcbin.connect_pad_added(move |_wb, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };

        let caps = match pad.current_caps().or_else(|| None) {
            Some(c) => c,
            None => return,
        };

        if !caps.to_string().contains("application/x-rtp") {
            return;
        }

        let pad_name = pad.name().to_string();

        let appsink = gst::ElementFactory::make("appsink")
            .name(&format!("rtp_tap.{pad_name}"))
            .build()
            .expect("appsink create failed");

        let appsink = appsink
            .downcast::<gst_app::AppSink>()
            .expect("appsink downcast failed");

        appsink.set_property("emit-signals", &true);
        appsink.set_property("sync", &false);

        pipeline.add(appsink.upcast_ref::<gst::Element>()).ok();
        appsink
            .upcast_ref::<gst::Element>()
            .sync_state_with_parent()
            .ok();

        if let Some(sinkpad) = appsink.static_pad("sink") {
            let _ = pad.link(&sinkpad);
        }

        let tx2 = tx.clone();
        let caps2 = caps.clone();

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = match sink.pull_sample() {
                        Ok(s) => s,
                        Err(_) => return Err(gst::FlowError::Eos),
                    };
                    let buffer = match sample.buffer() {
                        Some(b) => b,
                        None => return Ok(gst::FlowSuccess::Ok),
                    };
                    let map = match buffer.map_readable() {
                        Ok(m) => m,
                        Err(_) => return Ok(gst::FlowSuccess::Ok),
                    };

                    let pkt = RtpPacket {
                        pad_name: pad_name.clone(),
                        caps: caps2.clone(),
                        data: Bytes::copy_from_slice(map.as_slice()),
                        meta: parse_rtp_meta(map.as_slice(), &caps2),
                        pts: buffer.pts(),
                        dts: buffer.dts(),
                        duration: buffer.duration(),
                    };

                    match tx2.try_send(pkt) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            warn!("dropping RTP packet from tap because downstream queue is full");
                        }
                        Err(TrySendError::Closed(_)) => return Err(gst::FlowError::Eos),
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    });

    Ok(RtpTap { rx })
}

/// Create an RTP injector: create one appsrc feeding webrtcbin sink_%u when requested.
/// In v1 we create a sink pad per distinct pad_name we see from publisher.
pub fn install_rtp_injector(
    pipeline: &gst::Pipeline,
    webrtcbin: &gst::Element,
    mut initial_streams: Vec<RtpStreamInfo>,
    on_video_keyframe_request: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<RtpInjector> {
    let (tx, mut rx) = mpsc::channel::<RtpPacket>(RTP_INJECTOR_BUFFER);

    // We'll run a GLib task by using a bus idle or just rely on caller to call this on gst thread.
    // For now, assume called on gst thread and spawn a glib future:
    let pipeline_weak = pipeline.downgrade();
    let webrtcbin_weak = webrtcbin.downgrade();

    let map = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::<
        String,
        gst_app::AppSrc,
    >::new()));
    let map2 = map.clone();

    let make_src = move
        |pipeline: &gst::Pipeline, webrtcbin: &gst::Element, name: &str| -> gst_app::AppSrc {
            let appsrc_el = gst::ElementFactory::make("appsrc")
                .name(&format!("rtp_in.{name}"))
                .build()
                .expect("appsrc create failed");
            let appsrc = appsrc_el
                .downcast::<gst_app::AppSrc>()
                .expect("appsrc downcast failed");
            appsrc.set_property("is-live", &true);
            appsrc.set_property("format", &gst::Format::Time);
            // Use local pipeline time for forwarded RTP so a late-joining subscriber
            // does not inherit the publisher pipeline running-time and stall playback.
            appsrc.set_property("do-timestamp", &true);
            pipeline.add(appsrc.upcast_ref::<gst::Element>()).ok();
            appsrc
                .upcast_ref::<gst::Element>()
                .sync_state_with_parent()
                .ok();

            let preferred_pad = match name {
                "audio" => Some("sink_0"),
                "video" => Some("sink_1"),
                _ => None,
            };
            let sinkpad = preferred_pad
                .and_then(|pad_name| find_unlinked_pad_by_name(webrtcbin, pad_name))
                .or_else(|| {
                    preferred_pad.and_then(|pad_name| {
                        // Avoid requesting a named sink pad that already exists.
                        // GStreamer warns that behavior is undefined in that case.
                        if find_any_pad_by_name(webrtcbin, pad_name).is_none() {
                            webrtcbin.request_pad_simple(pad_name)
                        } else {
                            None
                        }
                    })
                })
                .or_else(|| webrtcbin.request_pad_simple("sink_%u"));
            let Some(sinkpad) = sinkpad else {
                warn!(
                    media_key = %name,
                    "failed to request webrtcbin sink pad; dropping stream injection for this key"
                );
                return appsrc;
            };
            let Some(srcpad) = appsrc.static_pad("src") else {
                warn!(
                    media_key = %name,
                    "appsrc has no static src pad; dropping stream injection for this key"
                );
                return appsrc;
            };
            if name == "video" {
                if let Some(on_video_keyframe_request) = on_video_keyframe_request.clone() {
                    srcpad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                        if info
                            .event()
                            .is_some_and(|event| event.has_name("GstForceKeyUnit"))
                        {
                            on_video_keyframe_request();
                        }
                        gst::PadProbeReturn::Ok
                    });
                }
            }
            if srcpad.link(&sinkpad).is_err() {
                // Some layouts expose sink_0/sink_1 but they are already occupied or otherwise unusable.
                // Retry with a newly requested sink_%u before giving up.
                let retry_sink = webrtcbin.request_pad_simple("sink_%u");
                if let Some(retry_sink) = retry_sink {
                    if srcpad.link(&retry_sink).is_err() {
                        warn!(
                            media_key = %name,
                            "failed to link appsrc to webrtcbin sink pad; dropping stream injection for this key"
                        );
                    }
                } else {
                    warn!(
                        media_key = %name,
                        "failed to link appsrc to webrtcbin sink pad; dropping stream injection for this key"
                    );
                }
            }

            appsrc
        };

    initial_streams.sort_by_key(|stream| match stream.media_key.as_str() {
        "audio" => 0,
        "video" => 1,
        _ => 2,
    });

    {
        let mut m = map.lock();
        for stream in initial_streams {
            let appsrc = make_src(pipeline, webrtcbin, &stream.media_key);
            appsrc.set_caps(Some(&stream.caps));
            m.insert(stream.media_key, appsrc);
        }
    }

    let main_context = glib::MainContext::ref_thread_default();
    main_context.spawn_local(async move {
        while let Some(pkt) = rx.recv().await {
            let (Some(pipeline), Some(webrtcbin)) =
                (pipeline_weak.upgrade(), webrtcbin_weak.upgrade())
            else {
                break;
            };

            let media_key = media_key(&pkt).unwrap_or_else(|| pkt.pad_name.clone());
            let appsrc = {
                let mut m = map2.lock();
                if let Some(src) = m.get(&media_key) {
                    src.clone()
                } else {
                    let appsrc = make_src(&pipeline, &webrtcbin, &media_key);
                    appsrc.set_caps(Some(&pkt.caps));
                    m.insert(media_key.clone(), appsrc.clone());
                    appsrc
                }
            };

            let mut buf = gst::Buffer::from_mut_slice(pkt.data.to_vec());
            {
                let b = buf.get_mut().unwrap();
                // Let appsrc timestamp on push; forwarded RTP packet payload already
                // carries RTP timestamps/seqnums from publisher.
                b.set_pts(gst::ClockTime::NONE);
                b.set_dts(gst::ClockTime::NONE);
                b.set_duration(gst::ClockTime::NONE);
            }
            let _ = appsrc.push_buffer(buf);
        }
    });

    Ok(RtpInjector { tx })
}

fn media_key(pkt: &RtpPacket) -> Option<String> {
    let caps = pkt.caps.to_string();
    if caps.contains("media=(string)audio") {
        Some("audio".to_string())
    } else if caps.contains("media=(string)video") {
        Some("video".to_string())
    } else {
        None
    }
}

fn parse_rtp_meta(pkt: &[u8], caps: &gst::Caps) -> RtpMeta {
    let Some(header) = parse_rtp_header(pkt) else {
        return RtpMeta::default();
    };

    let temporal_layer = if caps.to_string().contains("encoding-name=(string)VP8") {
        rtp_payload(pkt).and_then(vp8_temporal_layer)
    } else {
        None
    };

    RtpMeta {
        ssrc: Some(header.ssrc),
        sequence_number: Some(header.sequence_number),
        timestamp: Some(header.timestamp),
        temporal_layer,
    }
}

#[derive(Clone, Copy, Debug)]
struct RtpHeader {
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
}

fn parse_rtp_header(pkt: &[u8]) -> Option<RtpHeader> {
    if pkt.len() < 12 {
        return None;
    }
    let v = pkt[0] >> 6;
    if v != 2 {
        return None;
    }

    Some(RtpHeader {
        sequence_number: u16::from_be_bytes([pkt[2], pkt[3]]),
        timestamp: u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
        ssrc: u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]),
    })
}

fn find_unlinked_pad_by_name(webrtcbin: &gst::Element, name: &str) -> Option<gst::Pad> {
    webrtcbin
        .pads()
        .into_iter()
        .find(|pad| pad.name().as_str() == name && !pad.is_linked())
}

fn find_any_pad_by_name(webrtcbin: &gst::Element, name: &str) -> Option<gst::Pad> {
    webrtcbin
        .pads()
        .into_iter()
        .find(|pad| pad.name().as_str() == name)
}

pub fn stream_info(pkt: &RtpPacket) -> Option<RtpStreamInfo> {
    media_key(pkt).map(|media_key| RtpStreamInfo {
        media_key,
        caps: pkt.caps.clone(),
    })
}

pub fn is_probable_video_keyframe(pkt: &RtpPacket) -> bool {
    if !pkt.caps.to_string().contains("media=(string)video") {
        return false;
    }

    // v1: only inspect VP8 payloads; other codecs return false.
    if !pkt.caps.to_string().contains("encoding-name=(string)VP8") {
        return false;
    }

    let payload = match rtp_payload(&pkt.data) {
        Some(p) => p,
        None => return false,
    };

    is_vp8_keyframe(payload)
}

fn rtp_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 12 {
        return None;
    }
    let v = pkt[0] >> 6;
    if v != 2 {
        return None;
    }

    let cc = (pkt[0] & 0x0f) as usize;
    let has_ext = (pkt[0] & 0x10) != 0;
    let mut off = 12usize.checked_add(cc.checked_mul(4)?)?;
    if pkt.len() < off {
        return None;
    }

    if has_ext {
        if pkt.len() < off + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
        off = off.checked_add(4usize.checked_add(ext_words.checked_mul(4)?)?)?;
        if pkt.len() < off {
            return None;
        }
    }

    Some(&pkt[off..])
}

fn is_vp8_keyframe(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    // RFC 7741 VP8 payload descriptor.
    let b0 = payload[0];
    let x = (b0 & 0x80) != 0;
    let s = (b0 & 0x10) != 0;
    let part_id = b0 & 0x0f;
    let mut off = 1usize;

    if x {
        if payload.len() < off + 1 {
            return false;
        }
        let ext = payload[off];
        off += 1;

        if (ext & 0x80) != 0 {
            if payload.len() < off + 1 {
                return false;
            }
            let m = (payload[off] & 0x80) != 0;
            off += 1;
            if m {
                if payload.len() < off + 1 {
                    return false;
                }
                off += 1;
            }
        }
        if (ext & 0x40) != 0 {
            if payload.len() < off + 1 {
                return false;
            }
            off += 1;
        }
        if (ext & 0x20) != 0 || (ext & 0x10) != 0 {
            if payload.len() < off + 1 {
                return false;
            }
            off += 1;
        }
    }

    if !s || part_id != 0 || payload.len() <= off {
        return false;
    }

    // For VP8 payload header, frame type is bit0 (0 = keyframe, 1 = interframe).
    (payload[off] & 0x01) == 0
}

fn vp8_temporal_layer(payload: &[u8]) -> Option<u8> {
    if payload.is_empty() {
        return None;
    }

    let b0 = payload[0];
    let x = (b0 & 0x80) != 0;
    let mut off = 1usize;
    if !x {
        return None;
    }
    if payload.len() < off + 1 {
        return None;
    }

    let ext = payload[off];
    off += 1;

    if (ext & 0x80) != 0 {
        if payload.len() < off + 1 {
            return None;
        }
        let m = (payload[off] & 0x80) != 0;
        off += 1;
        if m {
            if payload.len() < off + 1 {
                return None;
            }
            off += 1;
        }
    }
    if (ext & 0x40) != 0 {
        if payload.len() < off + 1 {
            return None;
        }
        off += 1;
    }

    let has_tid = (ext & 0x20) != 0;
    let has_keyidx = (ext & 0x10) != 0;
    if !has_tid && !has_keyidx {
        return None;
    }
    if payload.len() < off + 1 {
        return None;
    }

    let tid_keyidx = payload[off];
    if has_tid {
        Some((tid_keyidx >> 6) & 0x03)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_rtp_header, vp8_temporal_layer};

    #[test]
    fn parses_basic_rtp_header() {
        let pkt = [
            0x80, 0x60, 0x12, 0x34, 0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44,
        ];
        let header = parse_rtp_header(&pkt).expect("valid header");
        assert_eq!(header.sequence_number, 0x1234);
        assert_eq!(header.timestamp, 0xaabbccdd);
        assert_eq!(header.ssrc, 0x11223344);
    }

    #[test]
    fn extracts_vp8_temporal_layer_from_extended_descriptor() {
        let payload = [
            0x90, // X=1, S=1
            0x20, // T=1
            0x80, // TID=2, Y=0, KEYIDX=0
            0x00, // start of VP8 payload header
        ];

        assert_eq!(vp8_temporal_layer(&payload), Some(2));
    }

    #[test]
    fn returns_none_when_vp8_temporal_layer_missing() {
        let payload = [0x10, 0x00];
        assert_eq!(vp8_temporal_layer(&payload), None);
    }
}
