//! Limits on how often clients may do what needs no account or guesses a secret: signing in
//! with the password, starting a QR login and registering a device; and on waking devices,
//! which makes other hosts broadcast on their LAN.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many clients a limiter tells apart. Past that, new ones share one allowance, so that a
/// flood of addresses cannot fill the memory.
const MAX_TRACKED: usize = 65_536;

/// Bits of an IPv6 address that name the client: a network hands out a whole /64 at a time.
const IPV6_CLIENT_BITS: u32 = 64;

/// A burst of `burst` attempts, after which one more is allowed every `every`.
#[derive(Debug, Clone, Copy)]
struct Rate {
    burst: u32,
    every: Duration,
}

impl Rate {
    /// How far ahead of now a client's allowance may be spent.
    fn tolerance(self) -> Duration {
        self.every * self.burst.saturating_sub(1)
    }
}

const PASSWORD_PER_CLIENT: Rate = Rate {
    burst: 5,
    every: Duration::from_secs(60),
};

/// Caps guessing from many addresses at once.
const PASSWORD_TOTAL: Rate = Rate {
    burst: 30,
    every: Duration::from_secs(10),
};

/// Clients start one when the last one expires, after three minutes.
const QR_LOGIN_PER_CLIENT: Rate = Rate {
    burst: 10,
    every: Duration::from_secs(10),
};

/// Clients register once, and every registration is kept for good.
const REGISTRATION_PER_CLIENT: Rate = Rate {
    burst: 10,
    every: Duration::from_secs(60),
};

const WAKE_PER_CLIENT: Rate = Rate {
    burst: 5,
    every: Duration::from_secs(30),
};

const SHARE_JOIN_PER_CLIENT: Rate = Rate {
    burst: 10,
    every: Duration::from_secs(30),
};

/// Counts attempts per client with the generic cell rate algorithm: each client has the time
/// at which its allowance is whole again.
#[derive(Debug)]
struct Limiter {
    rate: Rate,
    whole_at: Mutex<HashMap<Option<IpAddr>, Instant>>,
}

impl Limiter {
    fn new(rate: Rate) -> Self {
        Self {
            rate,
            whole_at: Mutex::default(),
        }
    }

    /// Counts an attempt by `client`, unless it has none left.
    fn admit(&self, client: Option<IpAddr>, now: Instant) -> bool {
        let mut whole_at = self.whole_at.lock().expect("limiter lock poisoned");
        if whole_at.len() >= MAX_TRACKED {
            whole_at.retain(|_, at| *at > now);
        }
        let client = client.map(client_network);
        let client = if whole_at.len() >= MAX_TRACKED && !whole_at.contains_key(&client) {
            None
        } else {
            client
        };
        let at = whole_at.get(&client).map_or(now, |at| (*at).max(now));
        if at.duration_since(now) > self.rate.tolerance() {
            return false;
        }
        whole_at.insert(client, at + self.rate.every);
        true
    }
}

/// The part of `address` that tells clients apart.
fn client_network(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => {
            let mask = u128::MAX << (128 - IPV6_CLIENT_BITS);
            IpAddr::V6((v6.to_bits() & mask).into())
        }
    }
}

/// Every limit, by what it limits. Clients whose address is unknown share one allowance.
#[derive(Debug)]
pub struct Limits {
    password: Limiter,
    password_total: Limiter,
    qr_login: Limiter,
    registration: Limiter,
    wake: Limiter,
    share_join: Limiter,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            password: Limiter::new(PASSWORD_PER_CLIENT),
            password_total: Limiter::new(PASSWORD_TOTAL),
            qr_login: Limiter::new(QR_LOGIN_PER_CLIENT),
            registration: Limiter::new(REGISTRATION_PER_CLIENT),
            wake: Limiter::new(WAKE_PER_CLIENT),
            share_join: Limiter::new(SHARE_JOIN_PER_CLIENT),
        }
    }
}

impl Limits {
    /// Counts a password sign-in by `client`, right or wrong; `false` if it may not try now.
    pub fn password(&self, client: Option<IpAddr>) -> bool {
        let now = Instant::now();
        let admitted = self.password.admit(client, now) && self.password_total.admit(None, now);
        if !admitted {
            tracing::debug!(limit = "password", "rate limited");
        }
        admitted
    }

    pub fn qr_login(&self, client: Option<IpAddr>) -> bool {
        Self::check(&self.qr_login, "qr_login", client)
    }

    pub fn registration(&self, client: Option<IpAddr>) -> bool {
        Self::check(&self.registration, "registration", client)
    }

    pub fn wake(&self, client: Option<IpAddr>) -> bool {
        Self::check(&self.wake, "wake", client)
    }

    pub fn share_join(&self, client: Option<IpAddr>) -> bool {
        Self::check(&self.share_join, "share_join", client)
    }

    fn check(limiter: &Limiter, limit: &'static str, client: Option<IpAddr>) -> bool {
        let admitted = limiter.admit(client, Instant::now());
        if !admitted {
            tracing::debug!(limit, "rate limited");
        }
        admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Rate = Rate {
        burst: 3,
        every: Duration::from_secs(10),
    };

    fn ip(address: &str) -> Option<IpAddr> {
        Some(address.parse().unwrap())
    }

    #[test]
    fn a_burst_then_one_per_period() {
        let limiter = Limiter::new(RATE);
        let start = Instant::now();
        let client = ip("192.0.2.1");
        for _ in 0..3 {
            assert!(limiter.admit(client, start));
        }
        assert!(!limiter.admit(client, start));
        assert!(!limiter.admit(client, start + Duration::from_secs(9)));
        assert!(limiter.admit(client, start + Duration::from_secs(10)));
        assert!(!limiter.admit(client, start + Duration::from_secs(10)));
        // Refused attempts cost nothing; waiting long enough makes the burst whole again.
        let later = start + Duration::from_secs(40);
        for _ in 0..3 {
            assert!(limiter.admit(client, later));
        }
        assert!(!limiter.admit(client, later));
    }

    #[test]
    fn clients_have_their_own_allowance() {
        let limiter = Limiter::new(Rate { burst: 1, ..RATE });
        let now = Instant::now();
        assert!(limiter.admit(ip("192.0.2.1"), now));
        assert!(!limiter.admit(ip("192.0.2.1"), now));
        assert!(limiter.admit(ip("192.0.2.2"), now));
        assert!(limiter.admit(None, now));
        assert!(!limiter.admit(None, now));
    }

    #[test]
    fn an_ipv6_client_is_its_network() {
        let limiter = Limiter::new(Rate { burst: 1, ..RATE });
        let now = Instant::now();
        assert!(limiter.admit(ip("2001:db8:0:1::1"), now));
        assert!(!limiter.admit(ip("2001:db8:0:1:ffff::2"), now));
        assert!(limiter.admit(ip("2001:db8:0:2::1"), now));
    }

    #[test]
    fn past_the_tracked_clients_new_ones_share() {
        let limiter = Limiter::new(Rate { burst: 1, ..RATE });
        let now = Instant::now();
        for n in 0..MAX_TRACKED - 1 {
            let address = IpAddr::from((n as u32).to_be_bytes());
            assert!(limiter.admit(Some(address), now));
        }
        assert!(limiter.admit(ip("192.0.2.1"), now));
        assert!(limiter.admit(ip("198.51.100.1"), now));
        assert!(!limiter.admit(ip("198.51.100.2"), now));
        // Clients whose allowance is whole again are forgotten to make room.
        let later = now + RATE.every;
        assert!(limiter.admit(ip("198.51.100.3"), later));
        assert_eq!(limiter.whole_at.lock().unwrap().len(), 1);
    }

    #[test]
    fn password_guesses_are_capped_in_total() {
        let limits = Limits::default();
        let mut admitted = 0;
        for n in 0..=255u8 {
            for _ in 0..PASSWORD_PER_CLIENT.burst {
                admitted += u32::from(limits.password(Some(IpAddr::from([192, 0, 2, n]))));
            }
        }
        assert_eq!(admitted, PASSWORD_TOTAL.burst);
    }
}
