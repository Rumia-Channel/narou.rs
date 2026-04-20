macro_rules! print {
    () => {{
        $crate::logger::emit_stdout("", false);
    }};
    ($($arg:tt)*) => {{
        $crate::logger::emit_stdout(&format!("{}", format_args!($($arg)*)), false);
    }};
}

macro_rules! println {
    () => {{
        $crate::logger::emit_stdout("", true);
    }};
    ($($arg:tt)*) => {{
        $crate::logger::emit_stdout(&format!("{}", format_args!($($arg)*)), true);
    }};
}

macro_rules! eprintln {
    () => {{
        $crate::logger::emit_stderr("", true);
    }};
    ($($arg:tt)*) => {{
        $crate::logger::emit_stderr(&format!("{}", format_args!($($arg)*)), true);
    }};
}
