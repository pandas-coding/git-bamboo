use git_workbench_protocol::{GraphPage, GraphViewport, Ref, StatusItem};

use crate::session::Session;

pub fn read_graph_page(
    _session: &Session,
    _viewport: &GraphViewport,
) -> anyhow::Result<GraphPage> {
    todo!()
}

pub fn read_status(
    _session: &Session,
    _paths: Option<&[String]>,
) -> anyhow::Result<Vec<StatusItem>> {
    todo!()
}

pub fn read_refs(_session: &Session) -> anyhow::Result<Vec<Ref>> {
    todo!()
}
