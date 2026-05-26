use super::atom_info::{AtomIdent, AtomInfo};
use super::ilst::Ilst;
use super::ilst::read::parse_ilst;
use super::read::{AtomReader, find_child_atom, meta_is_full, skip_atom};
use crate::config::{ParseOptions, ParsingMode};
use crate::error::Result;
use crate::macros::{decode_err, try_vec};
use crate::picture::{MimeType, Picture, PictureType};

use std::io::{Cursor, Read, Seek, SeekFrom};

use byteorder::{BigEndian, ReadBytesExt};

pub(crate) struct Moov {
	// Represents the trak.mdia atom
	pub(crate) traks: Vec<AtomInfo>,
	// Represents a parsed moov.udta.meta.ilst
	pub(crate) ilst: Option<Ilst>,
}

impl Moov {
	pub(super) fn find<R>(reader: &mut AtomReader<R>) -> Result<AtomInfo>
	where
		R: Read + Seek,
	{
		let mut moov = None;

		while let Ok(Some(atom)) = reader.next() {
			if atom.ident == AtomIdent::Fourcc(*b"moov") {
				moov = Some(atom);
				break;
			}

			skip_atom(reader, atom.extended, atom.len)?;
		}

		moov.ok_or_else(|| decode_err!(Mp4, "No \"moov\" atom found"))
	}

	pub(super) fn parse<R>(reader: &mut AtomReader<R>, parse_options: ParseOptions) -> Result<Self>
	where
		R: Read + Seek,
	{
		let mut traks = Vec::new();
		let mut ilst = None;

		while let Ok(Some(atom)) = reader.next() {
			if let AtomIdent::Fourcc(fourcc) = atom.ident {
				match &fourcc {
					b"trak" if parse_options.read_properties => {
						// All we need from here is trak.mdia
						if let Some(mdia) =
							find_child_atom(reader, atom.len, *b"mdia", parse_options.parsing_mode)?
						{
							skip_atom(reader, mdia.extended, mdia.len)?;
							traks.push(mdia);
						}
					},
					b"udta" if parse_options.read_tags => {
						let ilst_parsed = ilst_from_udta(reader, parse_options, atom.len - 8)?;
						if let Some(ilst_parsed) = ilst_parsed {
							let Some(mut existing_ilst) = ilst else {
								ilst = Some(ilst_parsed);
								continue;
							};

							log::warn!("Multiple `ilst` atoms found, combining them");
							for atom in ilst_parsed.atoms {
								existing_ilst.insert(atom);
							}

							ilst = Some(existing_ilst);
						}
					},
					_ => skip_atom(reader, atom.extended, atom.len)?,
				}

				continue;
			}

			skip_atom(reader, atom.extended, atom.len)?
		}

		Ok(Self { traks, ilst })
	}
}

fn ilst_from_udta<R>(
	reader: &mut AtomReader<R>,
	parse_options: ParseOptions,
	mut len: u64,
) -> Result<Option<Ilst>>
where
	R: Read + Seek,
{
	let mut ilst = None;

	while len > 8 {
		let Some(atom) = reader.next()? else {
			break;
		};

		len = len.saturating_sub(atom.len);

		match atom.ident {
			AtomIdent::Fourcc(fourcc) if fourcc == *b"meta" => {
				if let Some(parsed_ilst) = parse_ilst_from_meta(reader, parse_options, atom.len)? {
					merge_ilst(&mut ilst, parsed_ilst);
				}
			},
			AtomIdent::Fourcc(fourcc) if fourcc == *b"tags" && parse_options.read_cover_art => {
				let pictures = parse_cvrx_from_tags(reader, parse_options, atom.len)?;
				merge_cvrx_pictures(&mut ilst, pictures);
			},
			_ => skip_atom(reader, atom.extended, atom.len)?,
		}
	}

	Ok(ilst)
}

fn parse_ilst_from_meta<R>(
	reader: &mut AtomReader<R>,
	parse_options: ParseOptions,
	len: u64,
) -> Result<Option<Ilst>>
where
	R: Read + Seek,
{
	let meta_payload_start = reader.stream_position()?;
	let meta_payload_end = meta_payload_start + len.saturating_sub(8);

	// It's possible for the `meta` atom to be non-full,
	// so we have to check for that case.
	let full_meta_atom = meta_is_full(reader)?;
	let mut remaining = if full_meta_atom {
		len.saturating_sub(12)
	} else {
		len.saturating_sub(8)
	};

	let mut ilst = None;

	while remaining > 8 {
		let Some(atom) = reader.next()? else {
			break;
		};

		remaining = remaining.saturating_sub(atom.len);

		if atom.ident == AtomIdent::Fourcc(*b"ilst") {
			if let Some(parsed_ilst) = parse_ilst(reader, parse_options, atom.len - 8).map(Some)? {
				merge_ilst(&mut ilst, parsed_ilst);
			}
			continue;
		}

		skip_atom(reader, atom.extended, atom.len)?;
	}

	seek_to_absolute(reader, meta_payload_end)?;
	Ok(ilst)
}

fn parse_cvrx_from_tags<R>(
	reader: &mut AtomReader<R>,
	parse_options: ParseOptions,
	len: u64,
) -> Result<Vec<Picture>>
where
	R: Read + Seek,
{
	let tags_payload_start = reader.stream_position()?;
	let tags_payload_end = tags_payload_start + len.saturating_sub(8);
	let mut remaining = len.saturating_sub(8);
	let mut pictures = Vec::new();

	while remaining > 8 {
		let Some(atom) = reader.next()? else {
			break;
		};

		remaining = remaining.saturating_sub(atom.len);

		if atom.ident == AtomIdent::Fourcc(*b"cvrx") {
			let mut parsed_pictures = parse_cvrx(reader, parse_options, atom.len - 8)?;
			pictures.append(&mut parsed_pictures);
			continue;
		}

		skip_atom(reader, atom.extended, atom.len)?;
	}

	seek_to_absolute(reader, tags_payload_end)?;
	Ok(pictures)
}

fn parse_cvrx<R>(
	reader: &mut AtomReader<R>,
	parse_options: ParseOptions,
	len: u64,
) -> Result<Vec<Picture>>
where
	R: Read + Seek,
{
	let mut contents = try_vec![0; len as usize];
	reader.read_exact(&mut contents)?;

	let mut cursor = Cursor::new(contents);
	let entry_count = cursor.read_u64::<BigEndian>()?;
	let mut entries = Vec::new();

	for index in 0..entry_count as usize {
		let remaining = cursor.get_ref().len() as u64 - cursor.position();
		if remaining < 8 {
			if parse_options.parsing_mode == ParsingMode::Strict {
				decode_err!(@BAIL Mp4, "Incomplete `cvrx` entry header");
			}

			log::warn!("Stopping `cvrx` parse early due to truncated entry header");
			break;
		}

		let label_len = usize::from(cursor.read_u16::<BigEndian>()?);
		let required = label_len as u64 + 6;
		if required > cursor.get_ref().len() as u64 - cursor.position() {
			if parse_options.parsing_mode == ParsingMode::Strict {
				decode_err!(@BAIL Mp4, "Incomplete `cvrx` label or payload header");
			}

			log::warn!("Stopping `cvrx` parse early due to truncated label or payload header");
			break;
		}

		let mut label = try_vec![0; label_len];
		cursor.read_exact(&mut label)?;
		let _reserved = cursor.read_u32::<BigEndian>()?;
		let picture_len = usize::from(cursor.read_u16::<BigEndian>()?);
		if picture_len as u64 > cursor.get_ref().len() as u64 - cursor.position() {
			if parse_options.parsing_mode == ParsingMode::Strict {
				decode_err!(@BAIL Mp4, "Incomplete `cvrx` picture payload");
			}

			log::warn!("Stopping `cvrx` parse early due to truncated picture payload");
			break;
		}

		let mut picture_data = try_vec![0; picture_len];
		cursor.read_exact(&mut picture_data)?;

		entries.push((cvrx_label_rank(&label), index, picture_from_cvrx_data(picture_data)));
	}

	entries.sort_by_key(|(rank, index, _)| (*rank, *index));

	Ok(entries.into_iter().map(|(_, _, picture)| picture).collect())
}

fn merge_ilst(ilst: &mut Option<Ilst>, parsed: Ilst) {
	let Some(existing_ilst) = ilst.as_mut() else {
		*ilst = Some(parsed);
		return;
	};

	for atom in parsed.atoms {
		existing_ilst.insert(atom);
	}
}

fn merge_cvrx_pictures(ilst: &mut Option<Ilst>, pictures: Vec<Picture>) {
	if pictures.is_empty() {
		return;
	}

	let target = ilst.get_or_insert_with(Ilst::default);
	let existing_picture_count = target.pictures().map(|pictures| pictures.count()).unwrap_or(0);

	for picture in pictures.into_iter().skip(existing_picture_count) {
		target.insert_picture(picture);
	}
}

fn cvrx_label_rank(label: &[u8]) -> usize {
	if label.eq_ignore_ascii_case(b"front cover") {
		0
	} else if label.eq_ignore_ascii_case(b"back cover") {
		1
	} else if label.eq_ignore_ascii_case(b"artist") {
		2
	} else if label.eq_ignore_ascii_case(b"disc") {
		3
	} else if label.eq_ignore_ascii_case(b"icon") {
		4
	} else {
		usize::MAX
	}
}

fn picture_from_cvrx_data(data: Vec<u8>) -> Picture {
	let mime_type = sniff_cvrx_mime_type(&data);
	let mut picture = Picture::unchecked(data).pic_type(PictureType::Other);

	if let Some(mime_type) = mime_type {
		picture = picture.mime_type(mime_type);
	}

	picture.build()
}

fn sniff_cvrx_mime_type(data: &[u8]) -> Option<MimeType> {
	if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
		return Some(MimeType::Jpeg);
	}

	if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
		return Some(MimeType::Png);
	}

	if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
		return Some(MimeType::Gif);
	}

	if data.len() >= 2 && &data[..2] == b"BM" {
		return Some(MimeType::Bmp);
	}

	None
}

fn seek_to_absolute<R>(reader: &mut AtomReader<R>, target: u64) -> Result<()>
where
	R: Read + Seek,
{
	let current = reader.stream_position()?;
	if target > current {
		reader.seek(SeekFrom::Current((target - current) as i64))?;
	} else if current > target {
		reader.seek(SeekFrom::Current(-((current - target) as i64)))?;
	}

	Ok(())
}
