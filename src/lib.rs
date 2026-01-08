pub mod output;
pub mod server;
pub mod session;
pub mod shell;
pub mod task;

pub use output::{process_output, OutputBuffer, OutputFormat};
pub use server::McpServer;
pub use session::{CommandResult, SessionManager};
pub use shell::{detect_available_shells, detect_default_shell, Shell};
pub use task::{TaskManager, TaskStatus};
