use std::time::{Duration, Instant};

/// Per-connection state
#[derive(Debug, Clone)]
pub struct ConnectionState {
    /// The total amount of data (measured in octets or in packets) delivered so far over the lifetime of the transport connection
    delivered: u64,
    /// The wall clock time when [`ConnectionState::delivered`] was last updated
    delivered_time: Instant,
    /// Either:
    /// - If packets are in flight, then this holds the send time of the packet that was most recently marked as delivered.
    /// - Else, if the connection was recently idle, then this holds the send time of most recently sent packet.
    first_sent_time: Instant,
    /// Either:
    /// - The index of the last transmitted packet marked as application-limited,
    /// - or [`None`] if the connection is not currently application-limited.
    app_limited: Option<u64>,
}
impl ConnectionState {
    pub fn new(now: Instant) -> Self {
        Self {
            delivered: 0,
            delivered_time: now,
            first_sent_time: now,
            app_limited: None,
        }
    }

    /// Upon transmitting or retransmitting a data packet, the sender snapshots the current delivery information in per-packet state
    pub fn send_packet(
        &mut self,
        send_time: Instant,
        send_sequence_space: &TransportSendSequenceSpace,
    ) -> PacketState {
        if send_sequence_space.no_packets_in_flight() {
            self.first_sent_time = send_time;
            self.delivered_time = send_time;
        }
        PacketState {
            delivered: self.delivered,
            delivered_time: self.delivered_time,
            first_sent_time: self.first_sent_time,
            is_app_limited: self.app_limited.is_some(),
            sent_time: send_time,
        }
    }

    /// Upon transmitting or retransmitting a data packet, the sender snapshots the current delivery information in per-packet state
    pub fn send_packet_2(&mut self, send_time: Instant, no_packets_in_flight: bool) -> PacketState {
        if no_packets_in_flight {
            self.first_sent_time = send_time;
            self.delivered_time = send_time;
        }
        PacketState {
            delivered: self.delivered,
            delivered_time: self.delivered_time,
            first_sent_time: self.first_sent_time,
            is_app_limited: self.app_limited.is_some(),
            sent_time: send_time,
        }
    }

    /// Trigger situations:
    /// - the sending application asks the transport layer to send more data
    ///   - upon each write from the application, before new application data is enqueued in the transport send buffer or transmitted
    /// - `ACK` received from the transport layer
    ///   - at the beginning of `ACK` processing, before updating the estimated number of packets in flight, and before congestion control modifies the `cwnd` or pacing rate
    /// - timer
    ///   - at the beginning of connection timer processing, for all timers that might result in the transmission of one or more data segments
    ///   - e.g.: RTO timers, TLP timers, RACK reordering timers, Zero Window Probe timers
    pub fn detect_application_limited_phases(
        &mut self,
        sender_state: &ConnectionSenderState,
        send_sequence_space: &TransportSendSequenceSpace,
    ) {
        // the transport send buffer has less than `SMSS` of unsent data available to send
        let few_data_to_send =
            sender_state.write_seq - send_sequence_space.nxt < send_sequence_space.mss;
        // the amount of data considered in flight is less than the congestion window
        let cwnd_not_full = sender_state.pipe < send_sequence_space.wnd;

        if few_data_to_send
            && sender_state.not_transmitting_a_packet()
            && cwnd_not_full
            && sender_state.all_lost_packets_retransmitted()
        {
            let last_transmitted_packet_index = self.delivered + sender_state.pipe;
            self.app_limited = Some(last_transmitted_packet_index)
        }
    }

    /// Trigger situations:
    /// - the sending application asks the transport layer to send more data
    ///   - upon each write from the application, before new application data is enqueued in the transport send buffer or transmitted
    /// - `ACK` received from the transport layer
    ///   - at the beginning of `ACK` processing, before updating the estimated number of packets in flight, and before congestion control modifies the `cwnd` or pacing rate
    /// - timer
    ///   - at the beginning of connection timer processing, for all timers that might result in the transmission of one or more data segments
    ///   - e.g.: RTO timers, TLP timers, RACK reordering timers, Zero Window Probe timers
    pub fn detect_application_limited_phases_2(&mut self, params: DetectAppLimitedPhaseParams) {
        if !params.in_app_limited_phase() {
            return;
        }
        let last_transmitted_packet_index = self.delivered + params.pipe;
        self.app_limited = Some(last_transmitted_packet_index)
    }

    /// Upon receiving `ACK`
    ///
    /// `acked_packets` should not include already SACKed packets
    pub fn sample_rate(
        &mut self,
        acked_packets: &[Packet],
        now: Instant,
        min_rtt: Duration,
    ) -> Option<RateSample> {
        let mut prior_delivered = 0;
        struct PacketStats {
            prior_time: Instant,
            is_app_limited: bool,
            send_elapsed: Duration,
            ack_elapsed: Duration,
        }
        let mut newest_packet_stats = None;

        for packet in acked_packets {
            self.delivered += packet.data_length;
            self.delivered_time = now;
            // Update info using the newest packet
            if prior_delivered < packet.state.delivered {
                prior_delivered = packet.state.delivered;
                newest_packet_stats = Some(PacketStats {
                    prior_time: packet.state.delivered_time,
                    is_app_limited: packet.state.is_app_limited,
                    send_elapsed: packet.state.sent_time - packet.state.first_sent_time,
                    ack_elapsed: self.delivered_time - packet.state.delivered_time,
                });
                self.first_sent_time = packet.state.sent_time;
            }
        }

        // Clear app-limited field if bubble is ACKed and gone
        if let Some(app_limited) = self.app_limited {
            if app_limited < self.delivered {
                self.app_limited = None;
            }
        }

        // Nothing delivered on this ACK
        let prior_delivered = prior_delivered;
        let PacketStats {
            prior_time,
            is_app_limited,
            send_elapsed,
            ack_elapsed,
        } = newest_packet_stats?;

        // Use the longer of the `send_elapsed` and `ack_elapsed`
        let interval = send_elapsed.max(ack_elapsed);

        let delivered = self.delivered - prior_delivered;

        // No reliable sample
        //
        // Normally we expect interval >= MinRTT.
        // Note that rate may still be overestimated when a spuriously
        // retransmitted skb was first (s)acked because "interval"
        // is under-estimated (up to an RTT). However, continuously
        // measuring the delivery rate during loss recovery is crucial
        // for connections that suffer heavy or prolonged losses.
        if interval < min_rtt {
            return None;
        }

        if interval.is_zero() {
            return None;
        }

        let delivery_rate = delivered as f64 / interval.as_secs_f64();

        Some(RateSample {
            delivery_rate,
            is_app_limited,
            interval,
            delivered,
            prior_delivered,
            prior_time,
            send_elapsed,
            ack_elapsed,
        })
    }
}

/// Per-connection sender state
#[derive(Debug, Clone)]
pub struct ConnectionSenderState {
    /// The data sequence number one higher than that of the last octet queued for transmission in the transport layer write buffer.
    pub write_seq: u64,
    /// The number of bytes queued for transmission on the sending host at layers lower than the transport layer
    /// - the transport layer: i.e. network layer, traffic shaping layer, network device layer.
    pub pending_transmissions: u64,
    /// The number of packets in the current outstanding window that are marked as lost.
    /// - outstanding: still waiting for acknowledgement
    pub lost_out: u64,
    /// The number of packets in the current outstanding window that are being retransmitted.
    pub retrans_out: u64,
    /// The sender's estimate of the amount of data outstanding in the network (measured in octets or packets).
    /// - This includes data packets in the current outstanding window that are being transmitted or retransmitted and have not been SACKed or marked lost (e.g. "pipe" from [RFC6675]).
    /// - This does not include pure ACK packets.
    pub pipe: u64,
}
impl ConnectionSenderState {
    /// All the packets considered lost have been retransmitted
    fn all_lost_packets_retransmitted(&self) -> bool {
        self.lost_out <= self.retrans_out
    }

    /// The sending flow is not currently in the process of transmitting a packet
    fn not_transmitting_a_packet(&self) -> bool {
        self.pending_transmissions == 0
    }
}

/// ```text
///            1         2          3          4
///       ----------|----------|----------|----------
///              SND.UNA    SND.NXT    SND.UNA
///                                   +SND.WND
/// 1 - old sequence numbers which have been acknowledged
/// 2 - sequence numbers of unacknowledged data
/// 3 - sequence numbers allowed for new data transmission
/// 4 - future sequence numbers which are not yet allowed
///                   Send Sequence Space
/// ```
#[derive(Debug, Clone)]
pub struct TransportSendSequenceSpace {
    /// [`TransportSendSequenceSpace`]
    ///
    /// Measured in octets
    pub nxt: u64,
    /// [`TransportSendSequenceSpace`]
    ///
    /// Measured in octets
    pub una: u64,
    /// [`TransportSendSequenceSpace`]
    ///
    /// Measured in octets
    pub mss: u64,
    /// [`TransportSendSequenceSpace`]
    ///
    /// Measured in octets or packets
    pub wnd: u64,
}
impl TransportSendSequenceSpace {
    fn no_packets_in_flight(&self) -> bool {
        self.nxt == self.una
    }
}

/// Each packet that has been transmitted but not yet ACKed or SACKed.
///
/// A snapshot of connection delivery information from the time at which the packet was last transmitted.
#[derive(Debug, Clone)]
pub struct PacketState {
    /// [`ConnectionState::delivered`] when the packet was sent from the transport connection
    delivered: u64,
    /// [`ConnectionState::delivered_time`] when the packet was sent from the transport connection
    delivered_time: Instant,
    /// [`ConnectionState::first_sent_time`] when the packet was sent from the transport connection
    first_sent_time: Instant,
    /// True if [`ConnectionState::app_limited`] was [`Some`] when the packet was sent, else false
    is_app_limited: bool,
    /// The time when the packet was sent
    sent_time: Instant,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub state: PacketState,
    /// Measured in octets or packets
    pub data_length: u64,
}

#[derive(Debug, Clone)]
pub struct RateSample {
    delivery_rate: f64,
    is_app_limited: bool,
    interval: Duration,
    delivered: u64,
    prior_delivered: u64,
    prior_time: Instant,
    send_elapsed: Duration,
    ack_elapsed: Duration,
}
impl RateSample {
    /// The delivery rate sample
    pub fn delivery_rate(&self) -> f64 {
        self.delivery_rate
    }

    /// - The [`PacketState::is_app_limited`] from the most recent packet delivered
    /// - Indicates whether the rate sample is application-limited.
    pub fn is_app_limited(&self) -> bool {
        self.is_app_limited
    }

    /// The length of the sampling interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The amount of data marked as delivered over the sampling interval
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The [`PacketState::delivered`] count from the most recent packet delivered.
    pub fn prior_delivered(&self) -> u64 {
        self.prior_delivered
    }

    /// The [`PacketState::delivered_time`] from the most recent packet delivered.
    pub fn prior_time(&self) -> Instant {
        self.prior_time
    }

    /// Send time interval calculated from the most recent packet delivered
    pub fn send_elapsed(&self) -> Duration {
        self.send_elapsed
    }

    /// `ACK` time interval calculated from the most recent packet delivered
    pub fn ack_elapsed(&self) -> Duration {
        self.ack_elapsed
    }
}

#[derive(Debug, Clone)]
pub struct DetectAppLimitedPhaseParams {
    /// The transport send buffer has less than `SMSS` of unsent data available to send
    pub few_data_to_send: bool,
    /// The sending flow is not currently in the process of transmitting a packet
    pub not_transmitting_a_packet: bool,
    /// The amount of data considered in flight is less than the congestion window
    pub cwnd_not_full: bool,
    /// All the packets considered lost have been retransmitted
    pub all_lost_packets_retransmitted: bool,
    /// The sender's estimate of the amount of data outstanding in the network (measured in octets or packets).
    /// - This includes data packets in the current outstanding window that are being transmitted or retransmitted and have not been SACKed or marked lost (e.g. "pipe" from [RFC6675]).
    /// - This does not include pure ACK packets.
    pub pipe: u64,
}
impl DetectAppLimitedPhaseParams {
    fn in_app_limited_phase(&self) -> bool {
        self.few_data_to_send
            && self.not_transmitting_a_packet
            && self.cwnd_not_full
            && self.all_lost_packets_retransmitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_limited() {
        let now = Instant::now();
        let mut c = ConnectionState::new(now);
        let mut snd = TransportSendSequenceSpace {
            nxt: 0,
            una: 0,
            mss: 1,
            wnd: 2,
        };
        let mut c_s = ConnectionSenderState {
            write_seq: 0,
            pending_transmissions: 0,
            lost_out: 0,
            retrans_out: 0,
            pipe: 0,
        };

        // Application send
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.write_seq += 2;

        // Transport send
        let p_1 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;
        let p_2 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;

        // Transport recv
        let min_rtt = Duration::from_secs(1);
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_1,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(rs.is_none());
        snd.una += 1;

        // Transport recv
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_2,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(rs.is_none());
        snd.una += 1;

        // Application send
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.write_seq += 2;

        // Transport send
        let p_3 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;
        let p_4 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;

        // Transport recv
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_3,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(rs.unwrap().is_app_limited());
        snd.una += 1;

        // Transport recv
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_4,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(rs.unwrap().is_app_limited());
        snd.una += 1;
    }

    #[test]
    fn test_net_limited() {
        let now = Instant::now();
        let mut c = ConnectionState::new(now);
        let mut snd = TransportSendSequenceSpace {
            nxt: 0,
            una: 0,
            mss: 1,
            wnd: 1,
        };
        let mut c_s = ConnectionSenderState {
            write_seq: 0,
            pending_transmissions: 0,
            lost_out: 0,
            retrans_out: 0,
            pipe: 0,
        };

        // Application send
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.write_seq += 2;

        // Transport send
        let p_1 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;

        // Transport recv
        let min_rtt = Duration::from_secs(1);
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_1,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(rs.is_none());
        snd.una += 1;

        // Transport send
        let p_2 = c.send_packet(now, &snd);
        snd.nxt += 1;
        c_s.pipe += 1;

        // Transport recv
        let now = now + Duration::from_secs(1);
        c.detect_application_limited_phases(&c_s, &snd);
        c_s.pipe -= 1;
        let rs = c.sample_rate(
            &[Packet {
                state: p_2,
                data_length: 1,
            }],
            now,
            min_rtt,
        );
        dbg!(&rs);
        assert!(!rs.unwrap().is_app_limited());
        snd.una += 1;
    }
}
