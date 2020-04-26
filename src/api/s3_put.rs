use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::Arc;

use futures::stream::*;
use hyper::{Body, Request, Response};

use garage_util::data::*;
use garage_util::error::Error;
use garage_table::*;

use garage_core::block::INLINE_THRESHOLD;
use garage_core::block_ref_table::*;
use garage_core::garage::Garage;
use garage_core::object_table::*;
use garage_core::version_table::*;

use crate::http_util::*;

pub async fn handle_put(
	garage: Arc<Garage>,
	req: Request<Body>,
	bucket: &str,
	key: &str,
) -> Result<Response<BodyType>, Error> {
	let version_uuid = gen_uuid();
	let mime_type = get_mime_type(&req)?;
	let body = req.into_body();

	let mut chunker = BodyChunker::new(body, garage.config.block_size);
	let first_block = match chunker.next().await? {
		Some(x) => x,
		None => return Err(Error::BadRequest(format!("Empty body"))),
	};

	let mut object_version = ObjectVersion {
		uuid: version_uuid,
		timestamp: now_msec(),
		mime_type,
		size: first_block.len() as u64,
		state: ObjectVersionState::Uploading,
		data: ObjectVersionData::Uploading,
	};

	if first_block.len() < INLINE_THRESHOLD {
		object_version.data = ObjectVersionData::Inline(first_block);
		object_version.state = ObjectVersionState::Complete;

		let object = Object::new(bucket.into(), key.into(), vec![object_version]);
		garage.object_table.insert(&object).await?;
		return Ok(put_response(version_uuid));
	}

	let version = Version::new(version_uuid, bucket.into(), key.into(), false, vec![]);

	let first_block_hash = hash(&first_block[..]);
	object_version.data = ObjectVersionData::FirstBlock(first_block_hash);
	let object = Object::new(bucket.into(), key.into(), vec![object_version.clone()]);
	garage.object_table.insert(&object).await?;

	let total_size = read_and_put_blocks(&garage, version, 1, first_block, first_block_hash, &mut chunker).await?;

	// TODO: if at any step we have an error, we should undo everything we did

	object_version.state = ObjectVersionState::Complete;
	object_version.size = total_size;

	let object = Object::new(bucket.into(), key.into(), vec![object_version]);
	garage.object_table.insert(&object).await?;

	Ok(put_response(version_uuid))
}

async fn read_and_put_blocks(
	garage: &Arc<Garage>,
	version: Version,
	part_number: u64,
	first_block: Vec<u8>,
	first_block_hash: Hash,
	chunker: &mut BodyChunker,
) -> Result<u64, Error> {
	let mut next_offset = first_block.len();
	let mut put_curr_version_block =
		put_block_meta(garage.clone(), &version, part_number, 0, first_block_hash, first_block.len() as u64);
	let mut put_curr_block = garage
		.block_manager
		.rpc_put_block(first_block_hash, first_block);

	loop {
		let (_, _, next_block) =
			futures::try_join!(put_curr_block, put_curr_version_block, chunker.next())?;
		if let Some(block) = next_block {
			let block_hash = hash(&block[..]);
			let block_len = block.len();
			put_curr_version_block =
				put_block_meta(garage.clone(), &version, part_number, next_offset as u64, block_hash, block_len as u64);
			put_curr_block = garage.block_manager.rpc_put_block(block_hash, block);
			next_offset += block_len;
		} else {
			break;
		}
	}

	Ok(next_offset as u64)
}

async fn put_block_meta(
	garage: Arc<Garage>,
	version: &Version,
	part_number: u64,
	offset: u64,
	hash: Hash,
	size: u64,
) -> Result<(), Error> {
	// TODO: don't clone, restart from empty block list ??
	let mut version = version.clone();
	version
		.add_block(VersionBlock {
			part_number,
			offset,
			hash,
			size,
		})
		.unwrap();

	let block_ref = BlockRef {
		block: hash,
		version: version.uuid,
		deleted: false,
	};

	futures::try_join!(
		garage.version_table.insert(&version),
		garage.block_ref_table.insert(&block_ref),
	)?;
	Ok(())
}

struct BodyChunker {
	body: Body,
	read_all: bool,
	block_size: usize,
	buf: VecDeque<u8>,
}

impl BodyChunker {
	fn new(body: Body, block_size: usize) -> Self {
		Self {
			body,
			read_all: false,
			block_size,
			buf: VecDeque::new(),
		}
	}
	async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
		while !self.read_all && self.buf.len() < self.block_size {
			if let Some(block) = self.body.next().await {
				let bytes = block?;
				trace!("Body next: {} bytes", bytes.len());
				self.buf.extend(&bytes[..]);
			} else {
				self.read_all = true;
			}
		}
		if self.buf.len() == 0 {
			Ok(None)
		} else if self.buf.len() <= self.block_size {
			let block = self.buf.drain(..).collect::<Vec<u8>>();
			Ok(Some(block))
		} else {
			let block = self.buf.drain(..self.block_size).collect::<Vec<u8>>();
			Ok(Some(block))
		}
	}
}

fn put_response(version_uuid: UUID) -> Response<BodyType> {
	let resp_bytes = format!("{}\n", hex::encode(version_uuid));
	Response::new(Box::new(BytesBody::from(resp_bytes)))
}

pub async fn handle_create_multipart_upload(
	garage: Arc<Garage>,
	req: &Request<Body>,
	bucket: &str,
	key: &str,
) -> Result<Response<BodyType>, Error> {
	let version_uuid = gen_uuid();
	let mime_type = get_mime_type(req)?;

	let object_version = ObjectVersion {
		uuid: version_uuid,
		timestamp: now_msec(),
		mime_type,
		size: 0,
		state: ObjectVersionState::Uploading,
		data: ObjectVersionData::Uploading,
	};
	let object = Object::new(bucket.to_string(), key.to_string(), vec![object_version]);
	garage.object_table.insert(&object).await?;

	let mut xml = String::new();
	writeln!(&mut xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#).unwrap();
	writeln!(
		&mut xml,
		r#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#
	)
	.unwrap();
	writeln!(&mut xml, "\t<Bucket>{}</Bucket>", bucket).unwrap();
	writeln!(&mut xml, "\t<Key>{}</Key>", xml_escape(key)).unwrap();
	writeln!(
		&mut xml,
		"\t<UploadId>{}</UploadId>",
		hex::encode(version_uuid)
	)
	.unwrap();
	writeln!(&mut xml, "</InitiateMultipartUploadResult>").unwrap();

	Ok(Response::new(Box::new(BytesBody::from(xml.into_bytes()))))
}

pub async fn handle_put_part(
	garage: Arc<Garage>,
	req: Request<Body>,
	bucket: &str,
	key: &str,
	part_number_str: &str,
	upload_id: &str,
) -> Result<Response<BodyType>, Error> {
	// Check parameters
	let part_number = part_number_str
		.parse::<u64>()
		.map_err(|e| Error::BadRequest(format!("Invalid part number: {}", e)))?;

	let version_uuid = uuid_from_str(upload_id).map_err(|_| Error::BadRequest(format!("Invalid upload ID")))?;
	
	// Read first chuck, and at the same time try to get object to see if it exists
	let mut chunker = BodyChunker::new(req.into_body(), garage.config.block_size);

	let bucket = bucket.to_string();
	let key = key.to_string();
	let get_object_fut = garage.object_table.get(&bucket, &key);
	let get_first_block_fut = chunker.next();
	let (object, first_block) = futures::try_join!(get_object_fut, get_first_block_fut)?;

	// Check object is valid and multipart block can be accepted
	let first_block = match first_block {
		None => return Err(Error::BadRequest(format!("Empty body"))),
		Some(x) => x,
	};
	let object = match object {
		None => return Err(Error::BadRequest(format!("Object not found"))),
		Some(x) => x,
	};
	if !object.versions().iter().any(|v| {
		v.uuid == version_uuid
			&& v.state == ObjectVersionState::Uploading
			&& v.data == ObjectVersionData::Uploading
	}) {
		return Err(Error::BadRequest(format!(
			"Multipart upload does not exist or is otherwise invalid"
		)));
	}

	// Copy block to store
	let version = Version::new(version_uuid, bucket.into(), key.into(), false, vec![]);
	let first_block_hash = hash(&first_block[..]);
	read_and_put_blocks(&garage, version, part_number, first_block, first_block_hash, &mut chunker).await?;

	Ok(Response::new(Box::new(BytesBody::from(vec![]))))
}

pub async fn handle_complete_multipart_upload(
	garage: Arc<Garage>,
	_req: Request<Body>,
	bucket: &str,
	key: &str,
	upload_id: &str,
) -> Result<Response<BodyType>, Error> {
	let version_uuid = uuid_from_str(upload_id).map_err(|_| Error::BadRequest(format!("Invalid upload ID")))?;

	let bucket = bucket.to_string();
	let key = key.to_string();
	let (object, version) = futures::try_join!(
		garage.object_table.get(&bucket, &key),
		garage.version_table.get(&version_uuid, &EmptyKey),
	)?;
	let object = match object {
		None => return Err(Error::BadRequest(format!("Object not found"))),
		Some(x) => x,
	};
	let object_version = object.versions().iter().find(|v| {
		v.uuid == version_uuid
			&& v.state == ObjectVersionState::Uploading
			&& v.data == ObjectVersionData::Uploading
	});
	let mut object_version = match object_version {
		None => return Err(Error::BadRequest(format!(
			"Multipart upload does not exist or has already been completed"
		))),
		Some(x) => x.clone(),
	};
	let version = match version {
		None => return Err(Error::BadRequest(format!("Version not found"))),
		Some(x) => x,
	};
	if version.blocks().len() == 0 {
		return Err(Error::BadRequest(format!("No data was uploaded")));
	}

	// TODO: check that all the parts that they pretend they gave us are indeed there
	// TODO: check MD5 sum of all uploaded parts? but that would mean we have to store them somewhere...

	let total_size = version.blocks().iter().map(|x| x.size).fold(0, |x, y| x+y);
	object_version.size = total_size;
	object_version.state = ObjectVersionState::Complete;
	object_version.data = ObjectVersionData::FirstBlock(version.blocks()[0].hash);
	let final_object = Object::new(bucket.clone(), key.clone(), vec![object_version]);
	garage.object_table.insert(&final_object).await?;

	let mut xml = String::new();
	writeln!(&mut xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#).unwrap();
	writeln!(
		&mut xml,
		r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#
	)
	.unwrap();
	writeln!(&mut xml, "\t<Location>{}</Location>", garage.config.s3_api.s3_region).unwrap();
	writeln!(&mut xml, "\t<Bucket>{}</Bucket>", bucket).unwrap();
	writeln!(&mut xml, "\t<Key>{}</Key>", xml_escape(&key)).unwrap();
	writeln!(&mut xml, "</CompleteMultipartUploadResult>").unwrap();

	Ok(Response::new(Box::new(BytesBody::from(xml.into_bytes()))))
}

pub async fn handle_abort_multipart_upload(
	garage: Arc<Garage>,
	bucket: &str,
	key: &str,
	upload_id: &str,
) -> Result<Response<BodyType>, Error> {
	let version_uuid = uuid_from_str(upload_id).map_err(|_| Error::BadRequest(format!("Invalid upload ID")))?;

	let object = garage.object_table.get(&bucket.to_string(), &key.to_string()).await?;
	let object = match object {
		None => return Err(Error::BadRequest(format!("Object not found"))),
		Some(x) => x,
	};
	let object_version = object.versions().iter().find(|v| {
		v.uuid == version_uuid
			&& v.state == ObjectVersionState::Uploading
			&& v.data == ObjectVersionData::Uploading
	});
	let mut object_version = match object_version {
		None => return Err(Error::BadRequest(format!(
			"Multipart upload does not exist or has already been completed"
		))),
		Some(x) => x.clone(),
	};

	object_version.state = ObjectVersionState::Aborted;
	let final_object = Object::new(bucket.to_string(), key.to_string(), vec![object_version]);
	garage.object_table.insert(&final_object).await?;

	Ok(Response::new(Box::new(BytesBody::from(vec![]))))
}

fn get_mime_type(req: &Request<Body>) -> Result<String, Error> {
	Ok(req
		.headers()
		.get(hyper::header::CONTENT_TYPE)
		.map(|x| x.to_str())
		.unwrap_or(Ok("blob"))?
		.to_string())
}

fn uuid_from_str(id: &str) -> Result<UUID, ()> {
	let id_bin = hex::decode(id).map_err(|_| ())?;
	if id_bin.len() != 32 {
		return Err(());
	}
	let mut uuid = [0u8; 32];
	uuid.copy_from_slice(&id_bin[..]);
	Ok(UUID::from(uuid))
}

pub async fn handle_delete(garage: Arc<Garage>, bucket: &str, key: &str) -> Result<UUID, Error> {
	let object = match garage
		.object_table
		.get(&bucket.to_string(), &key.to_string())
		.await?
	{
		None => {
			// No need to delete
			return Ok([0u8; 32].into());
		}
		Some(o) => o,
	};

	let interesting_versions = object.versions().iter().filter(|v| {
		v.data != ObjectVersionData::DeleteMarker && v.state != ObjectVersionState::Aborted
	});

	let mut must_delete = false;
	let mut timestamp = now_msec();
	for v in interesting_versions {
		must_delete = true;
		timestamp = std::cmp::max(timestamp, v.timestamp + 1);
	}

	if !must_delete {
		return Ok([0u8; 32].into());
	}

	let version_uuid = gen_uuid();

	let object = Object::new(
		bucket.into(),
		key.into(),
		vec![ObjectVersion {
			uuid: version_uuid,
			timestamp: now_msec(),
			mime_type: "application/x-delete-marker".into(),
			size: 0,
			state: ObjectVersionState::Complete,
			data: ObjectVersionData::DeleteMarker,
		}],
	);

	garage.object_table.insert(&object).await?;
	return Ok(version_uuid);
}