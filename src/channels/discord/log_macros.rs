//! Timestamped log macros, path-accessible so every module can call them.

/// `eprintln!` with an RFC3339 UTC timestamp prefix.
macro_rules! eprintln_stamped {
    ($($arg:tt)*) => {
        ::std::eprintln!("{} {}", $crate::channels::discord::log_stamp::now(), ::std::format_args!($($arg)*))
    };
}
pub(crate) use eprintln_stamped;

/// `println!` with an RFC3339 UTC timestamp prefix.
macro_rules! println_stamped {
    ($($arg:tt)*) => {
        ::std::println!("{} {}", $crate::channels::discord::log_stamp::now(), ::std::format_args!($($arg)*))
    };
}
pub(crate) use println_stamped;
