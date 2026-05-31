use anyhow::{Result, Context, bail};
use std::sync::{Arc, Mutex};
use serde::{Serialize, Deserialize};

/// ⚓️ [V=1, ROLE: THE ARCHITECT, CORE: RUST_JACKTRIP]
/// STATUS: 10-SIGMA OXIDIZED JITTER REDUCTION
/// 
/// MANDATE: Reify JackTrip's audio-grade jitter buffer in pure Rust.
///          Abolish C++ non-determinism and Qt dependencies.

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct IOStat {
    pub underruns: u32,
    pub overflows: u32,
    pub skew: i32,
    pub skew_raw: i32,
    pub level: i32,
    pub buf_dec_overflows: u32,
    pub buf_dec_pktloss: u32,
    pub buf_inc_underrun: u32,
    pub buf_inc_compensate: u32,
    pub broadcast_skew: i32,
    pub broadcast_delta: i32,
    pub autoq_corr: i32,
    pub autoq_rate: i32,
}

pub struct JitterBuffer {
    slot_size: usize,
    total_size: usize,
    buffer: Vec<i8>,
    read_pos: i32,
    write_pos: i32,
    
    m_active: bool,
    m_max_latency: i32,
    m_level_cur: f64,
    m_level: i32,
    m_reads_new: u32,
    m_underruns_new: u32,
    
    // AutoQueue logic
    m_auto_queue: i32,
    m_auto_queue_corr: f64,
    m_auto_q_factor: f64,
    m_auto_q_rate: f64,
    m_auto_q_rate_min: f64,
    m_auto_q_rate_decay: f64,
    
    // Stats
    m_underruns: u32,
    m_overflows: u32,
    m_skew_raw: i32,
    m_buf_dec_overflow: u32,
    m_buf_dec_pkt_loss: u32,
    m_buf_inc_underrun: u32,
    m_buf_inc_compensate: u32,
    
    m_sample_rate: u32,
    m_fpp: u32,
    m_in_slot_size: usize,
    m_min_level_threshold: f64,
    m_last_corr_counter: u32,
    m_last_corr_direction: i32,
    
    m_underrun_inc_tolerance: f64,
    m_corr_inc_tolerance: f64,
    m_overflow_dec_tolerance: f64,
    m_overflow_drop_step: i32,
}

impl JitterBuffer {
    pub fn new(buf_samples: i32, qlen: i32, sample_rate: u32, strategy: i32, channels: u32, bit_res: u32) -> Self {
        let total_size = (sample_rate * channels * bit_res * 2) as usize;
        let slot_size = (buf_samples * channels as i32 * bit_res as i32) as usize;
        
        let mut jb = Self {
            slot_size,
            total_size,
            buffer: vec![0i8; total_size],
            read_pos: 0,
            write_pos: slot_size as i32, 
            
            m_active: false,
            m_max_latency: if qlen > 0 { qlen * slot_size as i32 } else { 3 * slot_size as i32 },
            m_level_cur: if qlen > 0 { (qlen * slot_size as i32) as f64 } else { (3 * slot_size as i32) as f64 },
            m_level: 0,
            m_reads_new: 0,
            m_underruns_new: 0,
            
            m_auto_queue: if qlen <= 0 { 1 } else { 0 },
            m_auto_queue_corr: 2.0 * slot_size as f64,
            m_auto_q_factor: if qlen < 0 { 1.0 / (-qlen as f64) } else { 1.0 / 500.0 },
            m_auto_q_rate: slot_size as f64 * 0.5,
            m_auto_q_rate_min: slot_size as f64 * 0.0005,
            m_auto_q_rate_decay: 1.0 - f64::min(buf_samples as f64 * 1.2e-6, 0.0005),
            
            m_underruns: 0,
            m_overflows: 0,
            m_skew_raw: 0,
            m_buf_dec_overflow: 0,
            m_buf_dec_pkt_loss: 0,
            m_buf_inc_underrun: 0,
            m_buf_inc_compensate: 0,
            
            m_sample_rate: sample_rate,
            m_fpp: buf_samples as u32,
            m_in_slot_size: slot_size,
            m_min_level_threshold: 1.9 * slot_size as f64,
            m_last_corr_counter: 0,
            m_last_corr_direction: 0,
            
            m_underrun_inc_tolerance: -10.0 * slot_size as f64,
            m_corr_inc_tolerance: 100.0 * (if qlen > 0 { qlen * slot_size as i32 } else { 3 * slot_size as i32 } as f64),
            m_overflow_dec_tolerance: 100.0 * (if qlen > 0 { qlen * slot_size as i32 } else { 3 * slot_size as i32 } as f64),
            m_overflow_drop_step: if qlen > 0 { qlen * slot_size as i32 } else { 3 * slot_size as i32 } / 2,
        };
        
        jb.m_level = jb.m_level_cur as i32;

        match strategy {
            1 => jb.m_overflow_drop_step = slot_size as i32,
            2 => {
                jb.m_underrun_inc_tolerance = 1.1 * slot_size as f64;
                jb.m_corr_inc_tolerance = 1.9 * slot_size as f64;
                jb.m_overflow_dec_tolerance = 0.1 * slot_size as f64;
                jb.m_overflow_drop_step = slot_size as i32;
            }
            _ => {}
        }
        
        jb
    }

    pub fn push(&mut self, data: &[i8], lost_len: i32) -> bool {
        let len = if data.is_empty() { self.slot_size } else { data.len() };
        self.m_in_slot_size = len;
        if !self.m_active { self.m_active = true; }
        
        if self.m_max_latency < (len as i32 + self.slot_size as i32) {
            self.m_max_latency = len as i32 + self.slot_size as i32;
        }
        
        if lost_len > 0 {
            self.process_packet_loss(lost_len);
        }
        
        self.m_skew_raw += (self.m_reads_new as i32).saturating_sub(len as i32);
        self.m_reads_new = 0;
        self.m_underruns += self.m_underruns_new;
        self.m_underruns_new = 0;
        self.m_level = (self.slot_size as i32) * ((self.m_level_cur / self.slot_size as f64).ceil() as i32);
        
        let available = self.write_pos - self.read_pos;
        let mut delta = 0;
        
        if available < -10 * self.m_max_latency {
            delta = available;
            self.m_buf_inc_underrun += (-delta) as u32;
            self.m_level_cur = len as f64;
        } else if (available + len as i32) > self.m_max_latency {
            delta = self.m_overflow_drop_step;
            self.m_overflows += delta as u32;
            self.m_buf_dec_overflow += delta as u32;
            self.m_level_cur = self.m_max_latency as f64;
        } else if available < 0 && self.m_level_cur < f64::max(self.m_in_slot_size as f64 + self.m_min_level_threshold, 
                                                              (self.m_max_latency as f64) - self.m_underrun_inc_tolerance - 2.0 * (self.slot_size as f64) * self.last_corr_factor()) {
            delta = -i32::min(-available, self.slot_size as i32);
            self.m_buf_inc_underrun += (-delta) as u32;
        } else if self.m_level_cur < ((self.m_max_latency as f64) - self.m_corr_inc_tolerance - 6.0 * (self.slot_size as f64) * self.last_corr_factor()) {
            delta = -(self.slot_size as i32);
            self.m_underruns += (-delta) as u32;
            self.m_buf_inc_compensate += (-delta) as u32;
        }
        
        if delta != 0 {
            self.read_pos += delta;
            self.m_last_corr_counter = 0;
            self.m_last_corr_direction = if delta > 0 { 1 } else { -1 };
        } else {
            self.m_last_corr_counter += 1;
        }
        
        let wpos = (self.write_pos.rem_euclid(self.total_size as i32)) as usize;
        let n = usize::min(self.total_size - wpos, len);
        if !data.is_empty() {
            self.buffer[wpos..wpos + n].copy_from_slice(&data[..n]);
            if n < len {
                self.buffer[..len - n].copy_from_slice(&data[n..]);
            }
        }
        self.write_pos += len as i32;
        
        true
    }

    pub fn pull(&mut self, out: &mut [i8]) {
        let len = self.slot_size;
        if !self.m_active {
            out.fill(0);
            return;
        }
        self.m_reads_new += len as u32;
        let available = self.write_pos - self.read_pos;
        
        if available < self.m_level_cur as i32 {
            self.m_level_cur = f64::max(available as f64, self.m_level_cur - (self.m_fpp as f64 / (5.0 * self.m_sample_rate as f64) * self.slot_size as f64));
        } else {
            self.m_level_cur = available as f64;
        }
        
        if (available as f64) + self.m_auto_queue_corr - self.m_level_cur < 0.0 {
            self.m_auto_queue_corr += self.m_auto_q_rate;
        } else if (self.m_in_slot_size as f64) + (self.slot_size as f64) < self.m_auto_queue_corr {
            self.m_auto_queue_corr -= self.m_auto_q_rate * self.m_auto_q_factor;
        }
        
        if self.m_auto_q_rate > self.m_auto_q_rate_min {
            self.m_auto_q_rate *= self.m_auto_q_rate_decay;
        }
        
        let read_len = i32::clamp(available, 0, len as i32) as usize;
        let rpos = (self.read_pos.rem_euclid(self.total_size as i32)) as usize;
        let n = usize::min(self.total_size - rpos, read_len);
        
        if read_len > 0 {
            out[..n].copy_from_slice(&self.buffer[rpos..rpos + n]);
            if n < read_len {
                out[n..read_len].copy_from_slice(&self.buffer[..read_len - n]);
            }
        }
        if read_len < len {
            out[read_len..].fill(0);
            self.m_underruns_new += (len - read_len) as u32;
        }
        self.read_pos += len as i32;
    }

    fn last_corr_factor(&self) -> f64 {
        500.0 / (u32::max(500, self.m_last_corr_counter) as f64)
    }

    fn process_packet_loss(&mut self, mut lost_len: i32) {
        self.m_skew_raw -= lost_len;
        let available = self.write_pos - self.read_pos;
        let delta = i32::min(available + self.m_in_slot_size as i32 + lost_len - self.m_max_latency, lost_len);
        
        if delta > 0 {
            lost_len -= delta;
            self.m_buf_dec_pkt_loss += delta as u32;
            self.m_level_cur = self.m_max_latency as f64;
            self.m_last_corr_counter = 0;
            self.m_last_corr_direction = 1;
        }
        
        if lost_len >= self.total_size as i32 {
            self.buffer.fill(0);
        } else if lost_len > 0 {
            let wpos = (self.write_pos.rem_euclid(self.total_size as i32)) as usize;
            let n = usize::min(self.total_size - wpos, lost_len as usize);
            self.buffer[wpos..wpos + n].fill(0);
            if n < lost_len as usize {
                self.buffer[..lost_len as usize - n].fill(0);
            }
        }
        self.write_pos += lost_len;
    }
}
