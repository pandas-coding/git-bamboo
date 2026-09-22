use std::sync::Arc;

use tower_lsp::Client;

use crate::session::Session;

pub struct WorkbenchService {
    pub session: Arc<Session>,
    pub client: Client,
}
