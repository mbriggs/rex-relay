//! Process exit codes. `send` and `wait` end with the code of the status the program stopped in.

/// Done, or a read-only command that succeeded.
pub const OK: i32 = 0;
/// Connection, protocol, or lookup failure.
pub const ERROR: i32 = 1;
/// Bad command-line arguments (clap's code), or a label that names more than one session.
pub const USAGE: i32 = 2;
/// A guard rail refused the request.
pub const REFUSED: i32 = 3;
/// The program is waiting for a permission approval, or shows a dialog such as folder trust.
pub const NEEDS_APPROVAL: i32 = 10;
/// The program asked a question.
pub const QUESTION: i32 = 11;
/// The program is waiting for input of an unspecified kind.
pub const NEEDS_INPUT: i32 = 12;
/// The program is waiting for a sign-in or other credential.
pub const NEEDS_SIGN_IN: i32 = 13;
/// The program reported an error.
pub const PROGRAM_ERROR: i32 = 14;
/// The program didn't stop before `--timeout`.
pub const TIMEOUT: i32 = 15;
/// The server couldn't be reached: the first connection failed for `connect_timeout`, or
/// `send` or `wait` lost the connection and couldn't restore it before `--timeout`. In the
/// second case the program's state is unknown.
pub const LOST_CONNECTION: i32 = 16;
