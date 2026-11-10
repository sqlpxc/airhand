//! broker 的锁：每把锁有一个等级（[`Rank`]），同时拿几把的时候只能从低往高拿。
//!
//! 两处代码按相反的顺序拿同样两把锁，就可能死锁；光靠注释记着顺序，加代码的时候很容易漏掉一把
//! （以前的注释只写了 hosts → reserved → bindings → waiters，`connect` 却在拿着 hosts 的时候去拿 pending）。
//! 所以调试构建（包括所有测试）里每次加锁都检查：这个线程手里不能已经有等级不低于它的锁，违反了直接 panic，
//! 测试里就能发现。发布构建里没有任何检查，就是普通的 [`std::sync::Mutex`]。
//!
//! 锁都是同步的、拿着它不 `await`（守卫不是 `Send`，拿着它的 future 也 spawn 不了），所以按线程记就够了。

use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard};

/// 锁的等级，从低到高：同时拿几把的时候按这个顺序（见模块说明）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Rank {
    /// 在线的被控端（[`super::registry`]）。`connect` 拿着它去拿 pending、waiters、failures：
    /// 拿着它检查控制码、预占会话槽位、登记会话、登记等待上线，中间不会有被控端上线或者下线
    Hosts,
    /// 掉线后保留着的控制码
    Reserved,
    /// 等两端的通道配对的会话（[`super::relay`]）
    Pending,
    /// 固定控制码的绑定表（[`crate::codes`]）
    Bindings,
    /// 等被控端上线的控制端
    Waiters,
    /// 建立了的会话，转发数据报用
    Routes,
    /// 猜错控制码的计数（[`super::limits`]）
    Failures,
    /// 新分配控制码的计数
    NewCodes,
    /// 每个来源有几条连接
    Conns,
}

impl Rank {
    #[cfg(debug_assertions)]
    const ALL: [Rank; 9] = [
        Rank::Hosts,
        Rank::Reserved,
        Rank::Pending,
        Rank::Bindings,
        Rank::Waiters,
        Rank::Routes,
        Rank::Failures,
        Rank::NewCodes,
        Rank::Conns,
    ];
}

#[cfg(debug_assertions)]
mod held {
    use super::Rank;
    use std::cell::Cell;

    thread_local! {
        /// 这个线程手里有哪些等级的锁（第 n 位是 `Rank` 的第 n 个）
        static HELD: Cell<u16> = const { Cell::new(0) };
    }

    /// 要拿 `rank`：手里不能有等级不低于它的锁
    pub(super) fn check(rank: Rank) {
        let held = HELD.get();
        let not_below = held & !((1u16 << rank as u16) - 1);
        if not_below != 0 {
            let holding: Vec<Rank> = Rank::ALL
                .into_iter()
                .filter(|r| held & (1 << *r as u16) != 0)
                .collect();
            panic!(
                "broker lock order violated: taking {rank:?} while holding {holding:?} \
                 (see the order in server/src/broker/lock.rs)"
            );
        }
    }

    pub(super) fn set(rank: Rank, on: bool) {
        let bit = 1u16 << rank as u16;
        HELD.set(if on {
            HELD.get() | bit
        } else {
            HELD.get() & !bit
        });
    }
}

/// 带等级的锁
pub(super) struct Ranked<T> {
    rank: Rank,
    inner: Mutex<T>,
}

impl<T> Ranked<T> {
    pub(super) fn new(rank: Rank, value: T) -> Self {
        Self {
            rank,
            inner: Mutex::new(value),
        }
    }

    pub(super) fn lock(&self) -> Guard<'_, T> {
        #[cfg(debug_assertions)]
        held::check(self.rank);
        let inner = self.inner.lock().expect("a broker lock is poisoned");
        // 拿到了才记下（加锁时 panic 的话，这个线程之后还会接着跑别的任务）
        #[cfg(debug_assertions)]
        held::set(self.rank, true);
        Guard {
            inner,
            rank: self.rank,
        }
    }
}

impl<T: Default> Ranked<T> {
    pub(super) fn default_with(rank: Rank) -> Self {
        Self::new(rank, T::default())
    }
}

pub(super) struct Guard<'a, T> {
    inner: MutexGuard<'a, T>,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    rank: Rank,
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        held::set(self.rank, false);
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn locks_in_order_are_fine() {
        let a = Ranked::new(Rank::Hosts, 1);
        let b = Ranked::new(Rank::Pending, 2);
        let c = Ranked::new(Rank::Conns, 3);
        {
            let _a = a.lock();
            let _b = b.lock();
            let _c = c.lock();
        }
        // 放开的顺序随意；放开之后再拿，不受之前的影响
        let ga = a.lock();
        let gb = b.lock();
        drop(ga);
        drop(gb);
        let _a = a.lock();
        let _c = c.lock();
    }

    #[test]
    #[should_panic(expected = "lock order violated")]
    fn reverse_order_panics() {
        let hosts = Ranked::new(Rank::Hosts, ());
        let pending = Ranked::new(Rank::Pending, ());
        let _p = pending.lock();
        let _h = hosts.lock();
    }

    #[test]
    #[should_panic(expected = "lock order violated")]
    fn the_same_rank_twice_panics() {
        let a = Ranked::new(Rank::Routes, ());
        let b = Ranked::new(Rank::Routes, ());
        let _a = a.lock();
        let _b = b.lock();
    }

    /// panic 之后（例如 should_panic 的测试、被 tokio 接住的任务），这个线程不会一直以为自己还拿着锁
    #[test]
    fn a_panic_releases_the_rank() {
        let hosts = Ranked::new(Rank::Hosts, ());
        let pending = Ranked::new(Rank::Pending, ());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _p = pending.lock();
            panic!("boom");
        }));
        assert!(r.is_err());
        let _h = hosts.lock();
    }
}
