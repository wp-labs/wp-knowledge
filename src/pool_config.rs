use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct CommonPoolConfig {
    pool_size: u32,
    min_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Option<Duration>,
    max_lifetime: Option<Duration>,
}

impl Default for CommonPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: 8,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(10),
            idle_timeout: Some(Duration::from_secs(60)),
            max_lifetime: None,
        }
    }
}

impl CommonPoolConfig {
    pub fn pool_size(&self) -> u32 {
        self.pool_size
    }

    pub fn min_connections(&self) -> u32 {
        self.min_connections.min(self.pool_size)
    }

    pub fn acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle_timeout
    }

    pub fn max_lifetime(&self) -> Option<Duration> {
        self.max_lifetime
    }

    pub fn with_pool_size(mut self, pool_size: Option<u32>) -> Self {
        if let Some(pool_size) = pool_size.filter(|size| *size > 0) {
            self.pool_size = pool_size;
            self.min_connections = self.min_connections.min(pool_size);
        }
        self
    }

    pub fn with_min_connections(mut self, min_connections: Option<u32>) -> Self {
        if let Some(min_connections) = min_connections {
            self.min_connections = min_connections.min(self.pool_size);
        }
        self
    }

    pub fn with_acquire_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        if let Some(timeout_ms) = timeout_ms.filter(|value| *value > 0) {
            self.acquire_timeout = Duration::from_millis(timeout_ms);
        }
        self
    }

    pub fn with_idle_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        if let Some(timeout_ms) = timeout_ms {
            self.idle_timeout = if timeout_ms == 0 {
                None
            } else {
                Some(Duration::from_millis(timeout_ms))
            };
        }
        self
    }

    pub fn with_max_lifetime_ms(mut self, timeout_ms: Option<u64>) -> Self {
        if let Some(timeout_ms) = timeout_ms {
            self.max_lifetime = if timeout_ms == 0 {
                None
            } else {
                Some(Duration::from_millis(timeout_ms))
            };
        }
        self
    }
}
