use serde::{Serialize, Deserialize};

pub type UUID = [u8; 32];
pub type Hash = [u8; 32];


// Data management

#[derive(Debug, Serialize, Deserialize)]
pub struct SplitpointMeta {
	bucket: String,
	key: String,

	timestamp: u64,
	uuid: UUID,
	deleted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VersionMeta {
	bucket: String,
	key: String,

	timestamp: u64,
	uuid: UUID,
	deleted: bool,

	mime_type: String,
	size: u64,
	is_complete: bool,

	data: VersionData,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum VersionData {
	Inline(Vec<u8>),
	FirstBlock(Hash),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockMeta {
	version_uuid: UUID,
	offset: u64,
	hash: Hash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockReverseMeta {
	versions: Vec<UUID>,
	deleted_versions: Vec<UUID>,
}
