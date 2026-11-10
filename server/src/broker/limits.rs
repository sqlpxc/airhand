//! 限速：每个来源的连接数、猜错控制码的次数、新分配的控制码、向同一台被控端发起的会话，以及每台被控端同时有几个会话。
//!
//! "来源"按网络算，不按单个地址：IPv6 按 /64（[`limiter_key`]），新分配控制码按 /48（[`registration_key`]），
//! 换个地址绕不过去。

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::Broker;
use super::lock::{Rank, Ranked};

/// 失败尝试统计窗口与上限（防暴力猜码）。
pub(super) const FAIL_WINDOW: Duration = Duration::from_secs(10 * 60);
pub(super) const FAIL_LIMIT: usize = 20;

/// 新分配控制码的统计窗口（上限见 [`super::Config::new_codes_per_ip`]）
pub(super) const NEW_CODE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// 同一个来源向同一台被控端发起会话的统计窗口（上限见 [`super::Config::connects_per_minute`]）
pub(super) const CONNECT_WINDOW: Duration = Duration::from_secs(60);

/// 会话槽位，Drop 时自动归还。
pub(super) struct SlotGuard(Arc<AtomicUsize>);

impl SlotGuard {
    /// 预占被控端的一个会话槽位（`sessions` 是它现在的会话数）。返回槽位和占之前有几个：
    /// 超过上限时调用方直接丢掉槽位（随之归还）
    pub(super) fn take(sessions: &Arc<AtomicUsize>) -> (Self, usize) {
        let before = sessions.fetch_add(1, Ordering::AcqRel);
        (Self(sessions.clone()), before)
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一个来源的连接，Drop 时归还（见 [`super::Config::max_conns_per_ip`]）
pub(super) struct ConnSlot<'a> {
    conns: &'a Ranked<HashMap<IpAddr, usize>>,
    key: IpAddr,
}

impl Drop for ConnSlot<'_> {
    fn drop(&mut self) {
        let mut conns = self.conns.lock();
        if let Some(n) = conns.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                conns.remove(&self.key);
            }
        }
    }
}

/// 按来源统计一段时间内发生了几次（猜错的次数、新分配的控制码；向同一台被控端发起的会话按来源和控制码一起算）
pub(super) struct Limiter<K = IpAddr> {
    window: Duration,
    hits: Ranked<HashMap<K, VecDeque<Instant>>>,
}

impl<K: Hash + Eq> Limiter<K> {
    pub(super) fn new(rank: Rank, window: Duration) -> Self {
        Self {
            window,
            hits: Ranked::default_with(rank),
        }
    }

    pub(super) fn count(&self, key: K) -> usize {
        let now = Instant::now();
        let mut hits = self.hits.lock();
        let Some(v) = hits.get_mut(&key) else {
            return 0;
        };
        while v.front().is_some_and(|t| now - *t >= self.window) {
            v.pop_front();
        }
        v.len()
    }

    pub(super) fn hit(&self, key: K) {
        self.hits
            .lock()
            .entry(key)
            .or_default()
            .push_back(Instant::now());
    }

    pub(super) fn cleanup(&self) {
        let now = Instant::now();
        self.hits.lock().retain(|_, v| {
            while v.front().is_some_and(|t| now - *t >= self.window) {
                v.pop_front();
            }
            !v.is_empty()
        });
    }
}

/// 新分配控制码的限量按这个来源统计：IPv6 按 /48 归并（一个用户手里通常有一整个 /56 或 /48，
/// 按 /64 算的话换个子网就绕过去了；正常使用时新分配控制码很少，归并得粗一点也不会误伤）
pub(super) fn registration_key(ip: IpAddr) -> IpAddr {
    match limiter_key(ip) {
        IpAddr::V6(v6) => {
            let mut seg = v6.segments();
            seg[3..].fill(0);
            IpAddr::V6(seg.into())
        }
        v4 => v4,
    }
}

/// IPv6 按 /64 归并，防止换地址绕过限速。
pub(super) fn limiter_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut seg = v6.segments();
            seg[4..].fill(0);
            IpAddr::V6(seg.into())
        }
    }
}

impl Broker {
    /// 占用来源 `key` 的一个连接名额；满了返回 `None`
    pub(super) fn admit(&self, key: IpAddr) -> Option<ConnSlot<'_>> {
        let mut conns = self.conns.lock();
        let n = conns.entry(key).or_default();
        if *n >= self.cfg.max_conns_per_ip {
            return None;
        }
        *n += 1;
        Some(ConnSlot {
            conns: &self.conns,
            key,
        })
    }

    pub(super) fn failure_count(&self, ip: IpAddr) -> usize {
        self.failures.count(ip)
    }

    pub(super) fn record_failure(&self, ip: IpAddr) {
        self.failures.hit(ip);
    }
}
