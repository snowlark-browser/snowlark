// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Font files mapped once for the font database resvg lays out SVG and canvas text with.
//!
//! `fontdb` keeps the system fonts as file paths and maps a file again on every access to it: font
//! fallback asks each face whether it has a character, so one `fillText` opened, mapped and closed
//! hundreds of files. Mapping each file once, for the life of the database, makes those reads memory
//! reads.

/// Maps every font file of `db` once (each face of a file shares the mapping).
pub fn map_all(db: &mut fontdb::Database) {
    let ids: Vec<fontdb::ID> = db.faces().map(|f| f.id).collect();
    for id in ids {
        if !matches!(
            db.face(id).map(|f| &f.source),
            Some(fontdb::Source::File(_))
        ) {
            continue;
        }
        // SAFETY: the mapping is read-only and lives as long as the database. A mapped file must
        // not change under it: Windows refuses writes to a file with a mapped view, and system
        // fonts are replaced, not edited, by installers (Linux package managers write a new file
        // and rename it over the old one, which leaves the mapped file as it was).
        let _ = unsafe { db.make_shared_face_data(id) };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn faces_read_from_mapped_files() {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        assert!(!db.is_empty());
        super::map_all(&mut db);
        assert!(db
            .faces()
            .all(|f| !matches!(f.source, fontdb::Source::File(_))));
        let id = db.faces().next().unwrap().id;
        assert!(db.with_face_data(id, |data, _| !data.is_empty()).unwrap());
    }
}
