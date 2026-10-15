//! A value made on first use: a command's clients, so that a run with
//! nothing to send needs no token, client id or subprocess.

use anyhow::{Result, anyhow};

/// Holds `make` until the first [`Lazy::get`], then what it made.
pub struct Lazy<T, F> {
    value: Option<T>,
    make: Option<F>,
}

impl<T, F: FnOnce() -> Result<T>> Lazy<T, F> {
    pub fn new(make: F) -> Self {
        Lazy {
            value: None,
            make: Some(make),
        }
    }

    /// The value, made now if this is the first call. After `make` has
    /// failed, every call is an error.
    pub fn get(&mut self) -> Result<&mut T> {
        if self.value.is_none()
            && let Some(make) = self.make.take()
        {
            self.value = Some(make()?);
        }
        self.value
            .as_mut()
            .ok_or_else(|| anyhow!("making it failed earlier in this run"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_once_on_first_get() {
        let calls = std::cell::Cell::new(0);
        let mut lazy = Lazy::new(|| {
            calls.set(calls.get() + 1);
            Ok(7)
        });
        assert_eq!(calls.get(), 0);
        *lazy.get().unwrap() += 1;
        assert_eq!(*lazy.get().unwrap(), 8);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_failed_make_is_returned_then_every_later_get_fails() {
        let mut lazy: Lazy<u8, _> = Lazy::new(|| Err(anyhow!("no client id")));
        assert_eq!(lazy.get().unwrap_err().to_string(), "no client id");
        assert!(lazy.get().is_err());
    }
}
