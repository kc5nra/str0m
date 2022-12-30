use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use net_::{Id, DATAGRAM_MTU};
use packet::{DepacketizingBuffer, Packetized, PacketizingBuffer};
pub use rtp::MediaTime;
use rtp::{Extensions, Fir, FirEntry, NackEntry, Pli, Rid, Rtcp, RtcpFb, RtpHeader, SdesType};
use rtp::{SeqNo, Ssrc, SRTP_BLOCK_SIZE, SRTP_OVERHEAD};

pub use rtp::{Direction, Mid, Pt};
pub use sdp::{Codec, FormatParams};
use sdp::{MediaLine, MediaType, Msid, Simulcast};

mod codec;
pub use codec::{CodecConfig, CodecParams};

mod app;
pub(crate) use app::App;

mod receiver;
use receiver::ReceiverSource;

mod sender;
use sender::SenderSource;

mod register;

use crate::change::AddMedia;
use crate::util::already_happened;
use crate::{KeyframeRequestKind, MediaData, RtcError};

// How often we remove unused senders/receivers.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(10);

// Time between regular receiver reports.
// https://www.rfc-editor.org/rfc/rfc8829#section-5.1.2
// Should technically be 4 seconds according to spec, but libWebRTC
// expects video to be every second, and audio every 5 seconds.
const RR_INTERVAL_VIDEO: Duration = Duration::from_millis(1000);
const RR_INTERVAL_AUDIO: Duration = Duration::from_millis(5000);

fn rr_interval(audio: bool) -> Duration {
    if audio {
        RR_INTERVAL_AUDIO
    } else {
        RR_INTERVAL_VIDEO
    }
}

/// Audio or video media.
///
/// An m-line in SDP.
#[derive(Debug)]
pub struct Media {
    /// Three letter identifier of this m-line.
    mid: Mid,

    /// The index of this media line in the Session::media Vec.
    index: usize,

    /// Unique CNAME for use in Sdes RTCP packets.
    ///
    /// This is for _outgoing_ SDP. Incoming CNAME can be
    /// found in the `ssrc_info_rx`.
    cname: String,

    /// "Stream and track" identifiers.
    ///
    /// This is for _outgoing_ SDP. Incoming Msid details
    /// can be found in the `ssrc_info_rx`.
    msid: Msid,

    /// Audio eller video.
    kind: MediaKind,

    /// The extenions for this m-line.
    exts: Extensions,

    /// Current media direction.
    ///
    /// Can be altered via negotiation.
    dir: Direction,

    /// Negotiated codec parameters.
    ///
    /// The PT information from SDP.
    params: Vec<CodecParams>,

    /// Receiving sources (SSRC).
    ///
    /// These are created first time we observe the SSRC in an incoming RTP packet.
    /// Each source keeps track of packet loss, nack, reports etc. Receiving sources are
    /// cleaned up when we haven't received any data for the SSRC for a while.
    sources_rx: Vec<ReceiverSource>,

    /// Sender sources (SSRC).
    ///
    /// Created when we configure new m-lines via Changes API.
    sources_tx: Vec<SenderSource>,

    /// Last time we ran cleanup.
    last_cleanup: Instant,

    /// Last time we produced regular feedback (SR/RR).
    last_regular_feedback: Instant,

    /// Buffers for incoming data.
    ///
    /// Video samples are often fragmented over several RTP packets. These buffers reassembles
    /// the incoming RTP to full samples.
    buffers_rx: HashMap<(Pt, Option<Rid>), DepacketizingBuffer>,

    /// Buffers for outgoing data.
    ///
    /// When writing a sample we create a number of RTP packets to send. These buffers have the
    /// individual RTP data payload ready to send.
    buffers_tx: HashMap<Pt, PacketizingBuffer>,

    /// Queued resends.
    ///
    /// These have been scheduled via nacks.
    resends: VecDeque<Resend>,

    /// Whether the media line needs to be advertised in an event.
    pub(crate) need_open_event: bool,

    /// If we receive an rtcp request for a keyframe, this holds what kind.
    keyframe_request_rx: Option<(Option<Rid>, KeyframeRequestKind)>,

    /// If we are to send an rtcp request for a keyframe, this holds what kind.
    keyframe_request_tx: Option<(Ssrc, KeyframeRequestKind)>,

    /// Simulcast configuration, if set.
    simulcast: Option<Simulcast>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Types of media.
pub enum MediaKind {
    /// Audio media.
    Audio,
    /// Video media.
    Video,
}

impl Media {
    pub fn mid(&self) -> Mid {
        self.mid
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn cname(&self) -> &str {
        &self.cname
    }

    pub(crate) fn set_cname(&mut self, cname: String) {
        self.cname = cname;
    }

    pub(crate) fn msid(&self) -> &Msid {
        &self.msid
    }

    pub(crate) fn kind(&self) -> MediaKind {
        self.kind
    }

    pub(crate) fn set_exts(&mut self, exts: Extensions) {
        if self.exts != exts {
            info!("Set {:?} extensions: {:?}", self.mid, exts);
            self.exts = exts;
        }
    }

    pub(crate) fn exts(&self) -> &Extensions {
        &self.exts
    }

    pub fn direction(&self) -> Direction {
        self.dir
    }

    fn codec_by_pt(&self, pt: Pt) -> Option<&CodecParams> {
        self.params.iter().find(|c| c.pt() == pt)
    }

    pub fn codecs(&self) -> &[CodecParams] {
        &self.params
    }

    pub fn get_writer(&mut self, pt: Pt, rid: Option<Rid>) -> MediaWriter<'_> {
        let codec = { self.codec_by_pt(pt).map(|p| p.codec()) };

        MediaWriter {
            media: self,
            pt,
            rid,
            codec,
        }
    }

    // #[instrument(skip_all, fields(mid = %self.mid()))]
    pub fn request_keyframe(
        &mut self,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), RtcError> {
        let rx = self
            .sources_rx
            .iter()
            .find(|s| s.rid() == rid && !s.is_rtx())
            .ok_or(RtcError::NoReceiverSource)?;

        info!("Request keyframe ({:?}) for SSRC: {}", kind, rx.ssrc());
        self.keyframe_request_tx = Some((rx.ssrc(), kind));

        Ok(())
    }

    pub(crate) fn poll_packet(
        &mut self,
        now: Instant,
        exts: &Extensions,
        twcc: &mut u64,
    ) -> Option<(RtpHeader, Vec<u8>, SeqNo)> {
        let (pt, pkt, ssrc, seq_no, orig_seq_no) = loop {
            if let Some(resend) = self.resends.pop_front() {
                // If there is no buffer for this resend, we loop to next. This is
                // a weird situation though, since it means the other side sent a nack for
                // an SSRC that matched this Media, but didnt match a buffer_tx.
                let buffer = match self.buffers_tx.values().find(|p| p.has_ssrc(resend.ssrc)) {
                    Some(v) => v,
                    None => continue,
                };

                // The seq_no could simply be too old to exist in the buffer, in which
                // case we will not do a resend.
                let pkt = match buffer.get(resend.seq_no) {
                    Some(v) => v,
                    None => continue,
                };

                // The send source, to get a contiguous seq_no for the resend.
                // Audio should not be resent, so this also gates whether we are doing resends at all.
                let source = match get_source_tx(&mut self.sources_tx, pkt.rid, true) {
                    Some(v) => v,
                    None => continue,
                };

                let seq_no = source.next_seq_no(now);

                // The resend ssrc. This would correspond to the RTX PT for video.
                let ssrc_rtx = source.ssrc();

                let orig_seq_no = Some(resend.seq_no);

                // Check that our internal state of organizing SSRC for senders is correct.
                assert_eq!(pkt.ssrc, resend.ssrc);
                assert_eq!(source.repairs(), Some(resend.ssrc));

                break (resend.pt, pkt, ssrc_rtx, seq_no, orig_seq_no);
            } else {
                // exit via ? here is ok since that means there is nothing to send.
                let (pt, pkt) = next_send_buffer(&mut self.buffers_tx)?;

                let source = self
                    .sources_tx
                    .iter_mut()
                    .find(|s| s.ssrc() == pkt.ssrc)
                    .expect("SenderSource for packetized write");

                let seq_no = source.next_seq_no(now);
                pkt.seq_no = Some(seq_no);

                break (pt, pkt, pkt.ssrc, seq_no, None);
            }
        };

        let mut header = RtpHeader::new(pt, seq_no, pkt.ts, ssrc);
        header.marker = pkt.last;

        // We can fill out as many values we want here, only the negotiated ones will
        // be used when writing the RTP packet.
        //
        // These need to match `Extension::is_supported()` so we are sending what we are
        // declaring we support.
        header.ext_vals.abs_send_time = Some(now.into());
        header.ext_vals.mid = Some(self.mid);
        header.ext_vals.transport_cc = Some(*twcc as u16);
        *twcc += 1;

        let mut buf = vec![0; 2000];
        let header_len = header.write_to(&mut buf, exts);
        assert!(header_len % 4 == 0, "RTP header must be multiple of 4");
        header.header_len = header_len;

        let mut body_out = &mut buf[header_len..];

        // For resends, the original seq_no is inserted before the payload.
        if let Some(orig_seq_no) = orig_seq_no {
            let n = RtpHeader::write_original_sequence_number(body_out, orig_seq_no);
            body_out = &mut body_out[n..];
        }

        let body_len = pkt.data.len();
        body_out[..body_len].copy_from_slice(&pkt.data);

        // pad for SRTP
        let pad_len = RtpHeader::pad_packet(&mut buf[..], header_len, body_len, SRTP_BLOCK_SIZE);

        buf.truncate(header_len + body_len + pad_len);

        Some((header, buf, seq_no))
    }

    pub(crate) fn get_source_rx(
        &mut self,
        header: &RtpHeader,
        now: Instant,
        do_update_receivers: bool,
    ) -> &mut ReceiverSource {
        // If we do_update_receivers, we want to know which SSRC the `rid_repair` header corresponds
        // to, we must figure this out before get_or_create_source_rx to fulfil the borrow checker.
        let repairs_ssrc = match (do_update_receivers, header.ext_vals.rid_repair) {
            (true, Some(id)) => self
                .sources_rx
                .iter()
                .find(|r| r.rid() == Some(id))
                .map(|r| r.ssrc()),
            _ => None,
        };

        let source = self.get_or_create_source_rx(header.ssrc, now);

        if do_update_receivers {
            if let Some(repairs) = repairs_ssrc {
                if source.repairs().is_none() {
                    source.set_repairs(repairs);
                }
            }

            if let Some(rid) = header.ext_vals.rid {
                if source.rid().is_none() {
                    source.set_rid(rid);
                }
            }
        }

        source
    }

    fn get_or_create_source_rx(&mut self, ssrc: Ssrc, now: Instant) -> &mut ReceiverSource {
        let maybe_idx = self.sources_rx.iter().position(|s| s.ssrc() == ssrc);

        if let Some(idx) = maybe_idx {
            &mut self.sources_rx[idx]
        } else {
            let new_source = ReceiverSource::new(ssrc, now);
            self.sources_rx.push(new_source);
            self.sources_rx.last_mut().unwrap()
        }
    }

    pub(crate) fn maybe_add_source_tx(&mut self, ssrc: Ssrc, repairs: Option<Ssrc>) {
        let maybe_idx = self.sources_tx.iter().position(|r| r.ssrc() == ssrc);

        let s = if let Some(idx) = maybe_idx {
            &mut self.sources_tx[idx]
        } else {
            self.sources_tx.push(SenderSource::new(ssrc));
            self.sources_tx.last_mut().unwrap()
        };

        if let Some(repairs) = repairs {
            if s.repairs().is_none() {
                s.set_repairs(repairs);
            }
        }
    }

    pub(crate) fn get_params(&self, header: &RtpHeader) -> Option<&CodecParams> {
        let pt = header.payload_type;
        self.params
            .iter()
            .find(|p| p.inner().codec.pt == pt || p.inner().resend == Some(pt))
    }

    pub(crate) fn has_nack(&mut self) -> bool {
        self.sources_rx
            .iter_mut()
            .filter(|s| !s.is_rtx())
            .any(|s| s.has_nack())
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        // TODO(martin): more cleanup
        self.last_cleanup = now;
    }

    pub(crate) fn poll_timeout(&mut self) -> Option<Instant> {
        Some(self.cleanup_at())
    }

    fn cleanup_at(&self) -> Instant {
        self.last_cleanup + CLEANUP_INTERVAL
    }

    pub(crate) fn first_source_tx(&self) -> Option<&SenderSource> {
        self.sources_tx.first()
    }

    pub(crate) fn source_tx_ssrcs(&self) -> impl Iterator<Item = Ssrc> + '_ {
        self.sources_tx.iter().map(|s| s.ssrc())
    }

    pub(crate) fn maybe_create_keyframe_request(&mut self, feedback: &mut VecDeque<Rtcp>) {
        let Some((ssrc, kind)) = self.keyframe_request_tx.take() else {
            return;
        };

        match kind {
            KeyframeRequestKind::Pli => feedback.push_back(Rtcp::Pli(Pli { ssrc })),
            KeyframeRequestKind::Fir => {
                // Unwrap is ok, because MediaWriter ensures the ReceiverSource exists.
                let rx = self
                    .sources_rx
                    .iter_mut()
                    .find(|s| s.ssrc() == ssrc)
                    .unwrap();

                feedback.push_back(Rtcp::Fir(Fir {
                    reports: FirEntry {
                        ssrc,
                        seq_no: rx.next_fir_seq_no(),
                    }
                    .into(),
                }));
            }
        }
    }

    /// Creates sender info and receiver reports for all senders/receivers
    pub(crate) fn maybe_create_regular_feedback(
        &mut self,
        now: Instant,
        feedback: &mut VecDeque<Rtcp>,
    ) -> Option<()> {
        if now < self.regular_feedback_at() {
            return None;
        }

        // If we don't have any sender sources, we can't create an SRTCP wrapper around the
        // feedback. This is because the SSRC is used to calculate the specific encryption key.
        // No sender SSRC, no encryption, no feedback possible.
        let first_ssrc = self.first_source_tx().map(|s| s.ssrc()).unwrap_or(0.into());

        // Since we're making new sender/receiver reports, clear out previous.
        feedback.retain(|r| !matches!(r, Rtcp::SenderReport(_) | Rtcp::ReceiverReport(_)));

        for s in &mut self.sources_tx {
            let sr = s.create_sender_report(now);
            let ds = s.create_sdes(&self.cname);

            debug!("Created feedback SR: {:?}", sr);
            feedback.push_back(Rtcp::SenderReport(sr));
            feedback.push_back(Rtcp::SourceDescription(ds));
        }

        for s in &mut self.sources_rx {
            let mut rr = s.create_receiver_report(now);
            rr.sender_ssrc = first_ssrc;

            debug!("Created feedback RR: {:?}", rr);
            feedback.push_back(Rtcp::ReceiverReport(rr));
        }

        // Update timestamp to move time when next is created.
        self.last_regular_feedback = now;

        Some(())
    }

    /// Creates nack reports for receivers, if needed.
    pub(crate) fn create_nack(&mut self, feedback: &mut VecDeque<Rtcp>) {
        for s in &mut self.sources_rx {
            if s.is_rtx() {
                continue;
            }
            if let Some(nack) = s.create_nack() {
                debug!("Created feedback NACK: {:?}", nack);
                feedback.push_back(nack);
            }
        }
    }

    /// Appply incoming RTCP feedback.
    pub(crate) fn handle_rtcp_fb(&mut self, now: Instant, fb: RtcpFb) -> Option<()> {
        trace!("Handle RTCP feedback: {:?}", fb);

        if fb.is_for_rx() {
            self.handle_rtcp_fb_rx(now, fb)?;
        } else {
            self.handle_rtcp_fb_tx(now, fb)?;
        }

        Some(())
    }

    pub(crate) fn handle_rtcp_fb_rx(&mut self, now: Instant, fb: RtcpFb) -> Option<()> {
        let ssrc = fb.ssrc();

        let source_rx = self.sources_rx.iter_mut().find(|s| s.ssrc() == ssrc)?;

        use RtcpFb::*;
        match fb {
            SenderInfo(v) => {
                source_rx.set_sender_info(now, v);
            }
            SourceDescription(v) => {
                for (sdes, st) in v.values {
                    if sdes == SdesType::CNAME {
                        if st.is_empty() {
                            // In simulcast, chrome doesn't send the SSRC lines, but
                            // expects us to infer that from rtp headers. It does
                            // however send the SourceDescription RTCP with an empty
                            // string CNAME. ¯\_(ツ)_/¯
                            return None;
                        }

                        // Here we _could_ check CNAME here matches something. But
                        // CNAMEs are a bit unfashionable with the WebRTC spec people.
                        return None;
                    }
                }
            }
            Goodbye(v) => {
                error!("Goodbye: {:?}", v);
            }
            _ => {}
        }

        Some(())
    }

    pub(crate) fn handle_rtcp_fb_tx(&mut self, _now: Instant, fb: RtcpFb) -> Option<()> {
        let ssrc = fb.ssrc();

        let source_tx = self.sources_tx.iter_mut().find(|s| s.ssrc() == ssrc)?;

        use RtcpFb::*;
        match fb {
            ReceptionReport(v) => {
                // TODO: What to do with these?
                trace!("Handle reception report: {:?}", v);
            }
            Nack(ssrc, list) => {
                let entries = list.into_iter();
                self.handle_nack(ssrc, entries)?;
            }
            Pli(_) => self.keyframe_request_rx = Some((source_tx.rid(), KeyframeRequestKind::Pli)),
            Fir(_) => self.keyframe_request_rx = Some((source_tx.rid(), KeyframeRequestKind::Fir)),
            Twcc(_) => unreachable!("TWCC should be handled on session level"),
            _ => {}
        }

        Some(())
    }

    pub(crate) fn apply_changes(
        &mut self,
        m: &MediaLine,
        config: &CodecConfig,
        session_exts: &Extensions,
    ) {
        // Directional changes
        {
            // All changes come from the other side, either via an incoming OFFER
            // or a ANSWER from our OFFER. Either way, the direction is inverted to
            // how we have it locally.
            let new_dir = m.direction().invert();
            if self.dir != new_dir {
                debug!(
                    "Mid ({}) change direction: {} -> {}",
                    self.mid, self.dir, new_dir
                );

                let was_receiving = self.dir.is_receiving();
                let was_sending = self.dir.is_sending();
                let is_receiving = new_dir.is_receiving();
                let is_sending = new_dir.is_sending();

                self.dir = new_dir;

                if was_receiving && !is_receiving {
                    // Receive buffers are dropped straight away.
                    self.clear_receive_buffers();
                }
                if !was_sending && is_sending {
                    // Dump the buffers when we are about to start sending. We don't do this
                    // on sending -> not, because we want to keep the buffer to answer straggle nacks.
                    self.clear_send_buffers();
                }
            }
        }

        // Changes in PT
        {
            let params: Vec<CodecParams> = m
                .rtp_params()
                .into_iter()
                .map(|m| m.into())
                .filter(|m| config.matches(m))
                .collect();
            let mut new_pts = HashSet::new();

            for p_new in params {
                new_pts.insert(p_new.pt());

                if let Some(p_old) = self.codec_by_pt(p_new.pt()) {
                    if *p_old != p_new {
                        debug!("Ignore change in mid ({}) for pt: {}", self.mid, p_new.pt());
                    }
                } else {
                    debug!("Ignoring new pt ({}) in mid: {}", p_new.pt(), self.mid);
                }
            }

            self.params.retain(|p| {
                let keep = new_pts.contains(&p.pt());

                if !keep {
                    debug!("Mid ({}) remove pt: {}", self.mid, p.pt());
                }

                keep
            });
        }

        // Update the extensions
        {
            let mut exts = Extensions::new();
            for x in m.extmaps() {
                exts.set_mapping(x);
            }
            exts.keep_same(session_exts);
            self.set_exts(exts);
        }

        // SSRC changes
        {
            let infos = m.ssrc_info();

            // Might want to update the data in already existing receivers
            for info in infos {
                if let Some(repairs) = info.repair {
                    self.get_or_create_source_rx(repairs, already_happened());
                }
                let r = self.get_or_create_source_rx(info.ssrc, already_happened());
                if let Some(repairs) = info.repair {
                    if r.repairs().is_none() {
                        r.set_repairs(repairs);
                    }
                }
            }
        }
    }

    pub(crate) fn has_ssrc_rx(&self, ssrc: Ssrc) -> bool {
        self.sources_rx.iter().any(|r| r.ssrc() == ssrc)
    }

    pub(crate) fn has_ssrc_tx(&self, ssrc: Ssrc) -> bool {
        self.sources_tx.iter().any(|r| r.ssrc() == ssrc)
    }

    pub(crate) fn get_buffer_rx(
        &mut self,
        pt: Pt,
        rid: Option<Rid>,
        codec: Codec,
    ) -> &mut DepacketizingBuffer {
        self.buffers_rx
            .entry((pt, rid))
            .or_insert_with(|| DepacketizingBuffer::new(codec.into(), 30))
    }

    pub(crate) fn poll_keyframe_request(&mut self) -> Option<(Option<Rid>, KeyframeRequestKind)> {
        self.keyframe_request_rx.take()
    }

    pub(crate) fn poll_sample(&mut self) -> Option<Result<MediaData, RtcError>> {
        for ((pt, rid), buf) in &mut self.buffers_rx {
            if let Some(r) = buf.pop() {
                let codec = self.params.iter().find(|c| c.pt() == *pt)?.clone();
                return Some(
                    r.map(|dep| MediaData {
                        mid: self.mid,
                        pt: *pt,
                        rid: *rid,
                        codec,
                        time: dep.time,
                        data: dep.data,
                        meta: dep.meta,
                    })
                    .map_err(|e| RtcError::Packet(self.mid, *pt, e)),
                );
            }
        }
        None
    }

    pub(crate) fn handle_nack(
        &mut self,
        ssrc: Ssrc,
        entries: impl Iterator<Item = NackEntry>,
    ) -> Option<()> {
        // Figure out which packetizing buffer has been used to send the entries that been nack'ed.
        let (pt, buffer) = self.buffers_tx.iter_mut().find(|(_, p)| p.has_ssrc(ssrc))?;

        // Turning NackEntry into SeqNo we need to know a SeqNo "close by" to lengthen the 16 bit
        // sequence number into the 64 bit we have in SeqNo.
        let seq_no = buffer.first_seq_no()?;
        let iter = entries.flat_map(|n| n.into_iter(seq_no));

        // Schedule all resends. They will be handled on next poll_packet
        self.resends.extend(iter.map(|seq_no| Resend {
            ssrc,
            pt: *pt,
            seq_no,
        }));

        Some(())
    }

    pub(crate) fn get_repaired_rx_ssrc(&self, ssrc: Ssrc) -> Option<Ssrc> {
        self.sources_rx
            .iter()
            .find(|r| r.ssrc() == ssrc)
            .and_then(|r| r.repairs())
    }

    pub(crate) fn clear_send_buffers(&mut self) {
        self.buffers_tx.clear();
    }

    pub(crate) fn clear_receive_buffers(&mut self) {
        self.buffers_rx.clear();
    }

    pub(crate) fn regular_feedback_at(&self) -> Instant {
        self.last_regular_feedback + rr_interval(self.kind == MediaKind::Audio)
    }

    pub fn match_codec(&self, codec: CodecParams) -> Option<Pt> {
        let c = self.params.iter().max_by_key(|c| c.match_score(codec))?;
        Some(c.pt())
    }

    pub(crate) fn simulcast(&self) -> Option<&Simulcast> {
        self.simulcast.as_ref()
    }

    pub(crate) fn set_simulcast(&mut self, s: Simulcast) {
        info!("Set simulcast: {:?}", s);
        self.simulcast = Some(s);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resend {
    pub ssrc: Ssrc,
    pub pt: Pt,
    pub seq_no: SeqNo,
}

fn next_send_buffer(
    buffers_tx: &mut HashMap<Pt, PacketizingBuffer>,
) -> Option<(Pt, &mut Packetized)> {
    for (pt, buf) in buffers_tx {
        if let Some(pkt) = buf.poll_next() {
            assert!(pkt.seq_no.is_none());
            return Some((*pt, pkt));
        }
    }
    None
}

impl Default for Media {
    fn default() -> Self {
        Self {
            mid: Mid::new(),
            index: 0,
            cname: Id::<20>::random().to_string(),
            msid: Msid {
                stream_id: Id::<30>::random().to_string(),
                track_id: Id::<30>::random().to_string(),
            },
            kind: MediaKind::Video,
            exts: Extensions::new(),
            dir: Direction::SendRecv,
            params: vec![],
            sources_rx: vec![],
            sources_tx: vec![],
            last_cleanup: already_happened(),
            last_regular_feedback: already_happened(),
            buffers_rx: HashMap::new(),
            buffers_tx: HashMap::new(),
            resends: VecDeque::new(),
            need_open_event: true,
            keyframe_request_rx: None,
            keyframe_request_tx: None,
            simulcast: None,
        }
    }
}

impl Media {
    pub(crate) fn from_remote_media_line(l: &MediaLine, index: usize, exts: Extensions) -> Self {
        Media {
            mid: l.mid(),
            index,
            kind: l.typ.clone().into(),
            exts,
            dir: l.direction().invert(), // remove direction is reverse.
            params: l.rtp_params().into_iter().map(|p| p.into()).collect(),
            ..Default::default()
        }
    }

    // Going from AddMedia to Media is for m-lines that are pending in a Change and are sent
    // in the offer to the other side.
    pub(crate) fn from_add_media(a: AddMedia, exts: Extensions) -> Self {
        let mut media = Media {
            mid: a.mid,
            index: a.index,
            cname: a.cname,
            msid: a.msid,
            kind: a.kind,
            exts,
            dir: a.dir,
            params: a.params,
            ..Default::default()
        };

        for (ssrc, repairs) in a.ssrcs {
            media.maybe_add_source_tx(ssrc, repairs);
        }

        media
    }
}

impl From<MediaType> for MediaKind {
    fn from(v: MediaType) -> Self {
        match v {
            MediaType::Audio => MediaKind::Audio,
            MediaType::Video => MediaKind::Video,
            _ => panic!("Not MediaType::Audio or Video"),
        }
    }
}

pub struct MediaWriter<'a> {
    media: &'a mut Media,
    pt: Pt,
    rid: Option<Rid>,
    codec: Option<Codec>,
}

impl MediaWriter<'_> {
    // #[instrument(skip_all, fields(mid = %self.media.mid()))]
    pub fn write(&mut self, ts: MediaTime, data: &[u8]) -> Result<usize, RtcError> {
        let codec = match self.codec {
            Some(v) => v,
            None => return Err(RtcError::UnknownPt(self.pt)),
        };

        if !self.media.dir.is_sending() {
            // Ignore any media writes while we are not sending.
            debug!("Ignore due to direction: {:?}", self.media.dir);
            return Ok(10_000);
        }

        // The SSRC is figured out given the simulcast level.
        let tx = get_source_tx(&mut self.media.sources_tx, self.rid, false)
            .ok_or(RtcError::NoSenderSource)?;

        let ssrc = tx.ssrc();

        let buf = self.media.buffers_tx.entry(self.pt).or_insert_with(|| {
            let max_retain = if codec.is_audio() { 4096 } else { 2048 };
            PacketizingBuffer::new(codec.into(), max_retain)
        });

        debug!("Write to packetizer time: {:?} bytes: {}", ts, data.len());
        if let Err(e) = buf.push_sample(ts, data, ssrc, self.rid, DATAGRAM_MTU - SRTP_OVERHEAD) {
            return Err(RtcError::Packet(self.media.mid, self.pt, e));
        };

        Ok(buf.free())
    }
}

/// Separate in wait for polonius.
fn get_source_tx(
    sources_tx: &mut Vec<SenderSource>,
    rid: Option<Rid>,
    is_rtx: bool,
) -> Option<&mut SenderSource> {
    sources_tx
        .iter_mut()
        .find(|s| rid == s.rid() && is_rtx == s.repairs().is_some())
}
