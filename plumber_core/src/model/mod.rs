mod mdl;
mod vtx;
mod vvd;

use std::{
    convert::TryInto,
    fmt::{self, Display},
    io,
    mem::size_of,
    result,
};

use mdl::Mdl;
pub use vtx::Face;
use vtx::Vtx;
use vvd::Vvd;
pub use vvd::{BoneWeight, Vertex};

use itertools::Itertools;
use thiserror::Error;

use crate::fs::{GameFile, OpenFileSystem, Path, PathBuf};

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("io error reading `{path}`: {kind:?}")]
    Io { path: String, kind: io::ErrorKind },
    #[error("not a {ty} file: invalid signature `{signature}`")]
    InvalidSignature { ty: FileType, signature: String },
    #[error("unsupported {ty} version {version}")]
    UnsupportedVersion { ty: FileType, version: i32 },
    #[error("{0} checksum doesn't match mdl checksum")]
    ChecksumMismatch(FileType),
    #[error("{ty} corrupted: {error}")]
    Corrupted { ty: FileType, error: &'static str },
}

#[derive(Debug, Clone)]
pub enum FileType {
    Mdl,
    Vvd,
    Vtx,
}

pub type Result<T> = result::Result<T, Error>;

impl Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FileType::Mdl => "mdl",
            FileType::Vvd => "vvd",
            FileType::Vtx => "vtx",
        })
    }
}

impl Error {
    fn from_io(err: &io::Error, path: &Path) -> Self {
        Self::Io {
            path: path.as_str().to_string(),
            kind: err.kind(),
        }
    }
}

const VTX_EXTENSIONS: &[&str] = &["dx90.vtx", "dx80.vtx", "sw.vtx", "vtx"];

fn find_vtx<'a>(
    mdl_path: &Path,
    file_system: &'a OpenFileSystem,
) -> Result<(PathBuf, GameFile<'a>)> {
    for &extension in VTX_EXTENSIONS {
        let path = mdl_path.with_extension(extension);
        match file_system.open_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(Error::from_io(&err, &path));
            }
        }
    }
    Err(Error::Io {
        path: mdl_path.with_extension("*.vtx").into_string(),
        kind: io::ErrorKind::NotFound,
    })
}

#[derive(Debug, Clone)]
pub struct Model {
    mdl: Mdl,
    vvd: Vvd,
    vtx: Vtx,
}

impl Model {
    /// # Errors
    ///
    /// Returns `Err` if reading the mdl file fails or if reading an associated vvd or vtx file fails.
    pub fn read(path: impl AsRef<Path>, file_system: &OpenFileSystem) -> Result<Self> {
        let path = path.as_ref();
        let mdl_file = file_system
            .open_file(path)
            .map_err(|err| Error::from_io(&err, path))?;
        let mdl = Mdl::read(mdl_file).map_err(|err| Error::from_io(&err, path))?;

        let vvd_path = path.with_extension("vvd");
        let vvd_file = file_system
            .open_file(&vvd_path)
            .map_err(|err| Error::from_io(&err, &vvd_path))?;
        let vvd = Vvd::read(vvd_file).map_err(|err| Error::from_io(&err, &vvd_path))?;

        let (vtx_path, vtx_file) = find_vtx(path, file_system)?;
        let vtx = Vtx::read(vtx_file).map_err(|err| Error::from_io(&err, &vtx_path))?;

        Ok(Model { mdl, vvd, vtx })
    }

    /// # Errors
    ///
    /// Returns `Err` if a signature or header is invalid or a version is unsupported.
    pub fn verify(&self) -> Result<Verified> {
        self.mdl.check_signature()?;
        self.mdl.check_version()?;

        self.vvd.check_signature()?;
        self.vvd.check_version()?;

        self.vtx.check_version()?;

        let mdl_header = self.mdl.header()?;
        let vvd_header = self.vvd.header()?;
        let vtx_header = self.vtx.header()?;

        if vvd_header.checksum() != mdl_header.checksum() {
            return Err(Error::ChecksumMismatch(FileType::Vvd));
        }
        if vtx_header.checksum() != mdl_header.checksum() {
            return Err(Error::ChecksumMismatch(FileType::Vtx));
        }

        Ok(Verified {
            mdl_header,
            vvd_header,
            vtx_header,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Verified<'a> {
    mdl_header: mdl::HeaderRef<'a>,
    vvd_header: vvd::HeaderRef<'a>,
    vtx_header: vtx::HeaderRef<'a>,
}

impl<'a> Verified<'a> {
    #[must_use]
    pub fn is_static_prop(&self) -> bool {
        self.mdl_header
            .flags()
            .contains(mdl::HeaderFlags::STATIC_PROP)
    }

    /// # Errors
    ///
    /// Returns `Err` if reading the name fails.
    pub fn name(&self) -> Result<&str> {
        self.mdl_header.name()
    }

    /// # Errors
    ///
    /// Returns `Err` if reading the meshes fails.
    pub fn meshes(&self) -> Result<Vec<Mesh>> {
        let vertices = self.vvd_header.vertices()?;

        let vtx_body_parts = self.vtx_header.iter_body_parts()?;
        let mdl_body_parts = self.mdl_header.iter_body_parts()?;

        let mut meshes = Vec::new();

        for (vtx_body_part, mdl_body_part) in vtx_body_parts.zip(mdl_body_parts) {
            let vtx_models = vtx_body_part.iter_models()?;
            let mdl_models = mdl_body_part.iter_models()?;

            let body_part_name = mdl_body_part.name()?;

            meshes.reserve(vtx_models.len());

            for (vtx_model, mdl_model) in vtx_models.zip(mdl_models) {
                let name = mdl_model.name()?;

                let vertex_offset: usize =
                    mdl_model
                        .vertex_offset
                        .try_into()
                        .map_err(|_| Error::Corrupted {
                            ty: FileType::Mdl,
                            error: "model vertex offset is negative",
                        })?;
                let vertex_count: usize =
                    mdl_model
                        .vertex_count
                        .try_into()
                        .map_err(|_| Error::Corrupted {
                            ty: FileType::Mdl,
                            error: "model vertex count is negative",
                        })?;

                if vertex_offset % size_of::<Vertex>() != 0 {
                    return Err(Error::Corrupted {
                        ty: FileType::Mdl,
                        error: "model vertex offset is misaligned",
                    });
                }

                let vertex_index = vertex_offset / size_of::<Vertex>();

                let model_vertices = vertices
                    .get(vertex_index..vertex_index + vertex_count)
                    .ok_or(Error::Corrupted {
                        ty: FileType::Mdl,
                        error: "model vertex offset out of bounds",
                    })?;

                let lods = vtx_model.lods()?;
                let lod_0 = if let Some(lod) = lods.get(0) {
                    lod
                } else {
                    continue;
                };

                let (vertice_indices, faces) = lod_0.merged_meshes(mdl_model)?;

                let vertices: Vec<_> = vertice_indices
                    .into_iter()
                    .map(|i| {
                        model_vertices.get(i).ok_or(Error::Corrupted {
                            ty: FileType::Vtx,
                            error: "vertice index out of bounds",
                        })
                    })
                    .try_collect()?;

                meshes.push(Mesh {
                    body_part_name,
                    name,
                    vertices,
                    faces,
                });
            }
        }

        Ok(meshes)
    }

    /// # Errors
    ///
    /// Returns `Err` if a material path reading fails or a material isn't found.
    pub fn materials(&self, file_system: &OpenFileSystem) -> Result<Vec<PathBuf>> {
        let texture_paths = self.mdl_header.texture_paths()?;

        self.mdl_header
            .iter_textures()?
            .map(|texture| find_material(texture, &texture_paths, file_system))
            .try_collect()
    }
}

fn find_material<'a>(
    texture: mdl::TextureRef,
    texture_paths: &[&str],
    file_system: &'a OpenFileSystem,
) -> Result<PathBuf> {
    let name = PathBuf::from(texture.name()?);

    for &path in texture_paths {
        let mut candidate = PathBuf::from("materials");
        candidate.push(PathBuf::from(path));
        candidate.push(&name);
        candidate.set_extension("vmt");

        match file_system.open_file(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(Error::from_io(&err, &candidate));
                }
            }
        }
    }

    Err(Error::Io {
        path: name.with_extension("vmt").into_string(),
        kind: io::ErrorKind::NotFound,
    })
}

#[derive(Debug, Clone)]
pub struct Mesh<'a> {
    pub body_part_name: &'a str,
    pub name: &'a str,
    pub vertices: Vec<&'a Vertex>,
    pub faces: Vec<Face>,
}

#[cfg(all(test, feature = "steam"))]
mod tests {
    use crate::{
        fs::{DirEntryType, OpenFileSystem, Path, ReadDir},
        steam::Libraries,
    };

    use super::*;

    /// Fails if steam is not installed
    #[test]
    #[ignore]
    fn read_models() {
        let libraries = Libraries::discover().unwrap();
        for result in libraries.apps().source().filesystems() {
            match result {
                Ok(filesystem) => {
                    eprintln!("reading from filesystem: {}", filesystem.name);
                    let filesystem = filesystem.open().unwrap();
                    recurse(
                        filesystem.read_dir(Path::try_from_str("models").unwrap()),
                        &filesystem,
                    );
                }
                Err(err) => eprintln!("warning: failed filesystem discovery: {}", err),
            }
        }
    }

    fn recurse(readdir: ReadDir, file_system: &OpenFileSystem) {
        for entry in readdir.map(result::Result::unwrap) {
            let name = entry.name();
            match entry.entry_type() {
                DirEntryType::File => {
                    if is_mdl_file(name.as_str()) {
                        if let Err(err) = read_mdl(&entry, file_system) {
                            if let Error::Corrupted { .. } = err {
                                panic!("failed: {:?}", err);
                            } else {
                                // ignore other errors, probably not our fault
                                eprintln!("failed: {:?}", err);
                            }
                        }
                    }
                }
                DirEntryType::Directory => recurse(entry.read_dir(), file_system),
            }
        }
    }

    fn read_mdl(entry: &crate::fs::DirEntry, file_system: &OpenFileSystem) -> Result<()> {
        let model = Model::read(entry.path(), file_system)?;
        let verified = model.verify()?;
        eprintln!("reading `{}`", verified.name()?);
        verified.meshes()?;
        verified.materials(file_system)?;
        Ok(())
    }

    fn is_mdl_file(filename: &str) -> bool {
        filename
            .rsplit('.')
            .next()
            .map(|ext| ext.eq_ignore_ascii_case("mdl"))
            == Some(true)
    }
}