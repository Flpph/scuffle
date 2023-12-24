use ulid::Ulid;

use crate::cli::display::TagResponse;
use crate::cli::{Cli, Invokable};
use crate::invoker::Invoker;

#[derive(Debug, clap::Args)]
pub struct Untag {
	/// The ids of the rooms to untag
	#[clap(long, required = true)]
	id: Ulid,

	/// The tags to remove from the room
	#[clap(long, value_parser, num_args = 1.., value_delimiter = ' ', required = true)]
	tags: Vec<String>,
}

#[async_trait::async_trait]
impl Invokable for Untag {
	async fn invoke(&self, invoker: &mut Invoker, _: &Cli) -> anyhow::Result<()> {
		let resp = invoker
			.invoke(pb::scuffle::video::v1::RoomUntagRequest {
				id: Some(self.id.into()),
				tags: self.tags.clone(),
			})
			.await?;

		invoker.display(&TagResponse::from((self.id, resp)))?;

		Ok(())
	}
}

impl From<(Ulid, pb::scuffle::video::v1::RoomUntagResponse)> for TagResponse {
	fn from((id, resp): (Ulid, pb::scuffle::video::v1::RoomUntagResponse)) -> Self {
		Self {
			id,
			tags: resp.tags.map(|tags| tags.tags).unwrap_or_default(),
		}
	}
}