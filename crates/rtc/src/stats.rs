//! Connection statistics for the debug panel: round-trip time, traffic and
//! packet loss, condensed from WebRTC's `getStats()` report.

use rtc::statistics::StatsSelector;
use rtc::statistics::report::RTCStatsReportEntry as Entry;
use rtc::statistics::stats::ice_candidate_pair::RTCStatsIceCandidatePairState;
use std::time::Instant;
use webrtc::peer_connection::PeerConnection;

/// One connection, as of the moment it was read. Counters are totals since
/// the connection started; callers diff them for rates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkStats {
    /// Network round trip, seconds (connectivity checks, else RTCP reports).
    pub rtt: Option<f64>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Receiving: RTP packets that arrived, and that went missing.
    pub packets_received: u64,
    pub packets_lost: i64,
    /// Sending: what the other side reports, from RTCP: packets it never
    /// got, and the share lost lately (0 to 1).
    pub remote_packets_lost: i64,
    pub remote_fraction_lost: Option<f64>,
}

/// Read the stats of `pc`.
pub async fn link_stats(pc: &impl PeerConnection) -> LinkStats {
    let report = pc.get_stats(Instant::now(), StatsSelector::None).await;
    let mut s = LinkStats::default();
    let mut rtcp_rtt = None;
    // Media bytes per RTP stream, summed; the candidate pair's counters can
    // come up short, so traffic is whichever is larger.
    let (mut rtp_in, mut rtp_out) = (0u64, 0u64);
    for entry in report.iter() {
        match entry {
            Entry::IceCandidatePair(pair) => {
                let active = pair.nominated || pair.state == RTCStatsIceCandidatePairState::Succeeded;
                if active && (pair.bytes_sent > 0 || pair.bytes_received > 0) {
                    s.bytes_sent = s.bytes_sent.max(pair.bytes_sent);
                    s.bytes_received = s.bytes_received.max(pair.bytes_received);
                    if pair.current_round_trip_time > 0.0 {
                        s.rtt = Some(pair.current_round_trip_time);
                    }
                }
            }
            Entry::InboundRtp(inbound) => {
                let r = &inbound.received_rtp_stream_stats;
                s.packets_received += r.packets_received;
                s.packets_lost += r.packets_lost.max(0);
                rtp_in += inbound.bytes_received + inbound.header_bytes_received;
            }
            Entry::OutboundRtp(outbound) => {
                rtp_out += outbound.sent_rtp_stream_stats.bytes_sent + outbound.header_bytes_sent;
            }
            Entry::RemoteInboundRtp(remote) => {
                s.remote_packets_lost += remote.received_rtp_stream_stats.packets_lost.max(0);
                let f = remote.fraction_lost;
                s.remote_fraction_lost = Some(s.remote_fraction_lost.map_or(f, |g: f64| g.max(f)));
                if remote.round_trip_time > 0.0 {
                    rtcp_rtt = Some(remote.round_trip_time);
                }
            }
            Entry::RemoteOutboundRtp(remote) if remote.round_trip_time > 0.0 => rtcp_rtt = Some(remote.round_trip_time),
            _ => {}
        }
    }
    s.rtt = s.rtt.or(rtcp_rtt);
    s.bytes_received = s.bytes_received.max(rtp_in);
    s.bytes_sent = s.bytes_sent.max(rtp_out);
    s
}
