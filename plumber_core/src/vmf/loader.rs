use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use plumber_vpk::PathBuf;
use rayon::{prelude::*, ThreadPoolBuilder};

use crate::asset::{Error, Handler, Importer};

pub use super::builder_utils::{GeometrySettings, InvisibleSolids, MergeSolids};
pub use super::overlay_builder::{BuiltOverlay, BuiltOverlayFace, OverlayError};
pub use super::prop_loader::{LoadedProp, PropError};
pub use super::solid_builder::{BuiltBrushEntity, BuiltSolid, Face, MergedSolids, SolidError};

pub(crate) use super::overlay_builder::SideFacesMap;

use super::{
    entities::{BaseEntity, TypedEntity},
    Vmf,
};

#[derive(Debug, Clone, Copy)]
pub enum GeometrySetting {
    Nothing,
    Brushes(GeometrySettings),
    BrushesAndOverlays(GeometrySettings),
}

impl Default for GeometrySetting {
    fn default() -> Self {
        Self::BrushesAndOverlays(GeometrySettings::default())
    }
}

impl GeometrySetting {
    #[must_use]
    pub fn brushes(&self) -> Option<&GeometrySettings> {
        if let Self::Brushes(s) | Self::BrushesAndOverlays(s) = self {
            Some(s)
        } else {
            None
        }
    }

    #[must_use]
    pub fn overlays(&self) -> Option<&GeometrySettings> {
        if let Self::BrushesAndOverlays(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    geometry: GeometrySetting,
    import_materials: bool,
    import_props: bool,
    import_entities: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            geometry: GeometrySetting::default(),
            import_materials: true,
            import_props: true,
            import_entities: true,
        }
    }
}

impl Settings {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn geometry(mut self, geometry: GeometrySetting) -> Self {
        self.geometry = geometry;
        self
    }

    #[must_use]
    pub fn import_materials(mut self, import_materials: bool) -> Self {
        self.import_materials = import_materials;
        self
    }

    #[must_use]
    pub fn import_props(mut self, import_props: bool) -> Self {
        self.import_props = import_props;
        self
    }

    #[must_use]
    pub fn import_entities(mut self, import_entities: bool) -> Self {
        self.import_entities = import_entities;
        self
    }
}

impl Vmf {
    pub(crate) fn load(
        &self,
        importer: Importer<impl Handler>,
        settings: &Settings,
        f: impl FnOnce(),
    ) {
        let side_faces_map = Arc::new(Mutex::new(BTreeMap::new()));
        self.send_material_jobs(&importer, settings.import_materials);

        let pool = build_thread_pool();
        pool.in_place_scope(|s| {
            s.spawn(|_| {
                if settings.import_props {
                    self.load_props(&importer).for_each_with(
                        importer.asset_handler.clone(),
                        |handler, r| match r {
                            Ok(prop) => handler.handle_prop(prop),
                            Err((id, error)) => handler.handle_error(Error::Prop { id, error }),
                        },
                    );
                }

                if settings.import_entities {
                    self.load_other_entities().for_each_with(
                        importer.asset_handler.clone(),
                        |handler, entity| {
                            handler.handle_entity(entity);
                        },
                    );
                }

                if let Some(geometry_settings) = settings.geometry.brushes() {
                    self.load_brushes(&importer, side_faces_map.clone(), *geometry_settings)
                        .for_each_with(importer.asset_handler.clone(), |handler, r| match r {
                            Ok(brush) => handler.handle_brush(brush),
                            Err((id, error)) => handler.handle_error(Error::Solid { id, error }),
                        });
                }

                if let Some(geometry_settings) = settings.geometry.overlays() {
                    self.load_overlays(side_faces_map, *geometry_settings)
                        .for_each_with(importer.asset_handler.clone(), |handler, r| match r {
                            Ok(overlay) => handler.handle_overlay(overlay),
                            Err((id, error)) => handler.handle_error(Error::Overlay { id, error }),
                        });
                }

                // drop the importer so that all copies of the asset handler are dropped,
                // possibly signaling the closure f to terminate as well
                drop(importer);
            });

            f();
        });
    }

    fn send_material_jobs(&self, importer: &Importer<impl Handler>, import_materials: bool) {
        // make sure solids' materials are loaded first
        // because solid loading later requires the material info to be available
        for solid in &self.world.solids {
            for side in &solid.sides {
                let mut material = PathBuf::from("materials");
                material.push(&side.material);
                importer.import_vmt(material);
            }
        }
        for entity in &self.entities {
            for solid in &entity.solids {
                for side in &solid.sides {
                    let mut material = PathBuf::from("materials");
                    material.push(&side.material);
                    importer.import_vmt(material);
                }
            }
        }

        if import_materials {
            // send overlay materials too so the material thread doesn't have to wait for other progress
            for entity in &self.entities {
                if let TypedEntity::Overlay(overlay) = entity.typed() {
                    match overlay.material() {
                        Ok(material) => {
                            let mut path = PathBuf::from("materials");
                            path.push(&material);
                            importer.import_vmt(material);
                        }
                        Err(err) => todo!("handle err: {}", err),
                    }
                }
            }
        }
    }

    fn load_props<'a>(
        &'a self,
        importer: &Importer<impl Handler>,
    ) -> impl ParallelIterator<Item = Result<LoadedProp<'a>, (i32, PropError)>> {
        self.entities
            .par_iter()
            .filter_map(|e| {
                if !e.solids.is_empty() {
                    return None;
                }
                let typed = e.typed();
                if let TypedEntity::Prop(prop) = typed {
                    Some(prop)
                } else {
                    None
                }
            })
            .map_with(
                (
                    importer.file_system.clone(),
                    importer.model_loader.clone(),
                    importer.material_job_sender.clone(),
                    importer.asset_handler.clone(),
                ),
                |(file_system, model_loader, material_job_sender, asset_handler), prop| {
                    let (prop, model) = prop
                        .load(model_loader, file_system)
                        .map_err(|(prop, e)| (prop.entity().id, e))?;

                    if let Some(model) = model {
                        for material in &model.materials {
                            material_job_sender
                                .send(material.clone())
                                .expect("material job channel shouldn't be disconnected");
                        }
                        asset_handler.handle_model(model);
                    }

                    Ok(prop)
                },
            )
    }

    fn load_other_entities(&self) -> impl ParallelIterator<Item = TypedEntity> {
        self.entities.par_iter().filter_map(|e| {
            if !e.solids.is_empty() {
                return None;
            }
            let typed = e.typed();
            // overlays and props are loaded separately
            if let TypedEntity::Overlay(..) | TypedEntity::Prop(..) = &typed {
                return None;
            }
            Some(typed)
        })
    }

    fn load_brushes<'a>(
        &'a self,
        importer: &Importer<impl Handler>,
        side_faces_map: Arc<Mutex<SideFacesMap>>,
        geometry_settings: GeometrySettings,
    ) -> impl ParallelIterator<Item = Result<BuiltBrushEntity<'a>, (i32, SolidError)>> {
        let world_brush_iter = rayon::iter::once(&self.world).map_with(
            (
                importer.material_loader.clone(),
                side_faces_map.clone(),
                geometry_settings,
            ),
            |(material_loader, side_faces_map, geometry_settings), world| {
                world
                    .build_brush(
                        |path| material_loader.wait_for_material(path),
                        side_faces_map,
                        geometry_settings,
                    )
                    .map_err(|e| (world.id, e))
            },
        );

        let entity_brushes_iter = {
            self.entities
                .par_iter()
                .filter(|entity| !entity.solids.is_empty())
                .map_with(
                    (
                        importer.material_loader.clone(),
                        side_faces_map,
                        geometry_settings,
                    ),
                    |(material_loader, side_faces_map, geometry_settings), entity| {
                        entity
                            .build_brush(
                                |path| material_loader.wait_for_material(path),
                                side_faces_map,
                                geometry_settings,
                            )
                            .map_err(|e| (entity.id, e))
                    },
                )
        };

        world_brush_iter.chain(entity_brushes_iter)
    }

    fn load_overlays(
        &self,
        side_faces_map: Arc<Mutex<SideFacesMap>>,
        geometry_settings: GeometrySettings,
    ) -> impl ParallelIterator<Item = Result<BuiltOverlay, (i32, OverlayError)>> {
        self.entities
            .par_iter()
            .filter_map(|e| {
                if let TypedEntity::Overlay(o) = e.typed() {
                    Some(o)
                } else {
                    None
                }
            })
            .map_with(
                (side_faces_map, geometry_settings),
                |(side_faces_map, geometry_settings), o| {
                    o.build_mesh(side_faces_map, geometry_settings)
                        .map_err(|(overlay, e)| (overlay.entity().id, e))
                },
            )
    }
}

fn build_thread_pool() -> rayon::ThreadPool {
    // this is 2 less than number of cpus since one thread is for material loading and one for asset callback
    // rest of the cpus are used for parallel asset loading
    let num_threads = num_cpus::get().saturating_sub(2).max(1);
    ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .expect("thread pool building shouldn't fail")
}
