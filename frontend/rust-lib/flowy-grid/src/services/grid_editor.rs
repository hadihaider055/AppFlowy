use crate::manager::GridUser;
use crate::services::kv_persistence::{GridKVPersistence, KVTransaction};
use crate::services::stringify::stringify_deserialize;

use dashmap::DashMap;
use flowy_collaboration::client_grid::{GridChange, GridMetaPad};
use flowy_collaboration::entities::revision::Revision;
use flowy_collaboration::util::make_delta_from_revisions;
use flowy_error::{FlowyError, FlowyResult};
use flowy_grid_data_model::entities::{
    Cell, CellMeta, Field, Grid, RepeatedField, RepeatedFieldOrder, RepeatedRow, RepeatedRowOrder, Row, RowMeta,
};
use flowy_sync::{RevisionCloudService, RevisionCompact, RevisionManager, RevisionObjectBuilder};
use lib_infra::future::FutureResult;
use lib_infra::uuid;
use lib_ot::core::PlainTextAttributes;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ClientGridEditor {
    grid_id: String,
    user: Arc<dyn GridUser>,
    grid_meta_pad: Arc<RwLock<GridMetaPad>>,
    rev_manager: Arc<RevisionManager>,
    kv_persistence: Arc<GridKVPersistence>,

    field_map: DashMap<String, Field>,
    cell_map: DashMap<String, CellMeta>,
}

impl ClientGridEditor {
    pub async fn new(
        grid_id: &str,
        user: Arc<dyn GridUser>,
        mut rev_manager: RevisionManager,
        kv_persistence: Arc<GridKVPersistence>,
    ) -> FlowyResult<Arc<Self>> {
        let token = user.token()?;
        let cloud = Arc::new(GridRevisionCloudService { token });
        let grid_pad = rev_manager.load::<GridPadBuilder, GridRevisionCompact>(cloud).await?;

        let rev_manager = Arc::new(rev_manager);
        let field_map = load_all_fields(&grid_pad, &kv_persistence).await?;
        let grid_meta_pad = Arc::new(RwLock::new(grid_pad));
        let cell_map = DashMap::new();

        Ok(Arc::new(Self {
            grid_id: grid_id.to_owned(),
            user,
            grid_meta_pad,
            rev_manager,
            kv_persistence,
            field_map,
            cell_map,
        }))
    }

    pub async fn create_empty_row(&self) -> FlowyResult<()> {
        let row = RowMeta::new(&uuid(), &self.grid_id, vec![]);
        self.create_row(row).await?;
        Ok(())
    }

    async fn create_row(&self, row: RowMeta) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.create_row(row)?)).await?;
        // self.cell_map.insert(row.id.clone(), row.clone());
        // let _ = self.kv_persistence.set(row)?;
        Ok(())
    }

    pub async fn delete_rows(&self, ids: Vec<String>) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.delete_rows(&ids)?)).await?;
        // let _ = self.kv.batch_delete(ids)?;
        Ok(())
    }

    // pub async fn update_row(&self, cell: Cell) -> FlowyResult<()> {
    //     match self.cell_map.get(&cell.id) {
    //         None => Err(FlowyError::internal().context(format!("Can't find cell with id: {}", cell.id))),
    //         Some(raw_cell) => {}
    //     }
    // }

    pub async fn create_field(&mut self, field: Field) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.create_field(field)?)).await?;
        Ok(())
    }

    pub async fn delete_field(&mut self, field_id: &str) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.delete_field(field_id)?)).await?;
        // let _ = self.kv.remove(field_id)?;
        Ok(())
    }

    pub async fn get_rows(&self, row_orders: RepeatedRowOrder) -> FlowyResult<RepeatedRow> {
        let ids = row_orders
            .items
            .into_iter()
            .map(|row_order| row_order.row_id)
            .collect::<Vec<_>>();
        let row_metas: Vec<RowMeta> = self.kv_persistence.batch_get(ids)?;

        let make_cell = |field_id: String, raw_cell: CellMeta| {
            let some_field = self.field_map.get(&field_id);
            if some_field.is_none() {
                tracing::error!("Can't find the field with {}", field_id);
                return None;
            }
            self.cell_map.insert(raw_cell.id.clone(), raw_cell.clone());

            let field = some_field.unwrap();
            match stringify_deserialize(raw_cell.data, field.value()) {
                Ok(content) => {
                    let cell = Cell {
                        id: raw_cell.id,
                        field_id: field_id.clone(),
                        content,
                    };
                    Some((field_id, cell))
                }
                Err(_) => None,
            }
        };

        let rows = row_metas
            .into_par_iter()
            .map(|row_meta| {
                let mut row = Row {
                    id: row_meta.id.clone(),
                    cell_by_field_id: Default::default(),
                    height: row_meta.height,
                };
                row.cell_by_field_id = row_meta
                    .cell_by_field_id
                    .into_par_iter()
                    .flat_map(|(field_id, raw_cell)| make_cell(field_id, raw_cell))
                    .collect::<HashMap<String, Cell>>();
                row
            })
            .collect::<Vec<Row>>();

        Ok(rows.into())
    }

    pub async fn get_fields(&self, field_orders: RepeatedFieldOrder) -> FlowyResult<RepeatedField> {
        let fields = field_orders
            .iter()
            .flat_map(|field_order| match self.field_map.get(&field_order.field_id) {
                None => {
                    tracing::error!("Can't find the field with {}", field_order.field_id);
                    None
                }
                Some(field) => Some(field.value().clone()),
            })
            .collect::<Vec<Field>>();
        Ok(fields.into())
    }

    pub async fn grid_data(&self) -> Grid {
        self.grid_meta_pad.read().await.grid_data()
    }

    pub async fn delta_str(&self) -> String {
        self.grid_meta_pad.read().await.delta_str()
    }

    async fn modify<F>(&self, f: F) -> FlowyResult<()>
    where
        F: for<'a> FnOnce(&'a mut GridMetaPad) -> FlowyResult<Option<GridChange>>,
    {
        let mut write_guard = self.grid_meta_pad.write().await;
        match f(&mut *write_guard)? {
            None => {}
            Some(change) => {
                let _ = self.apply_change(change).await?;
            }
        }
        Ok(())
    }

    async fn apply_change(&self, change: GridChange) -> FlowyResult<()> {
        let GridChange { delta, md5 } = change;
        let user_id = self.user.user_id()?;
        let (base_rev_id, rev_id) = self.rev_manager.next_rev_id_pair();
        let delta_data = delta.to_bytes();
        let revision = Revision::new(
            &self.rev_manager.object_id,
            base_rev_id,
            rev_id,
            delta_data,
            &user_id,
            md5,
        );
        let _ = self
            .rev_manager
            .add_local_revision::<GridRevisionCompact>(&revision)
            .await?;
        Ok(())
    }
}

async fn load_all_fields(
    grid_pad: &GridMetaPad,
    kv_persistence: &Arc<GridKVPersistence>,
) -> FlowyResult<DashMap<String, Field>> {
    let field_ids = grid_pad
        .fields()
        .iter()
        .map(|field| field.id.clone())
        .collect::<Vec<_>>();

    let fields = kv_persistence.batch_get::<Field>(field_ids)?;
    let map = DashMap::new();
    for field in fields {
        map.insert(field.id.clone(), field);
    }
    Ok(map)
}

struct GridPadBuilder();
impl RevisionObjectBuilder for GridPadBuilder {
    type Output = GridMetaPad;

    fn build_object(object_id: &str, revisions: Vec<Revision>) -> FlowyResult<Self::Output> {
        let pad = GridMetaPad::from_revisions(object_id, revisions)?;
        Ok(pad)
    }
}

struct GridRevisionCloudService {
    #[allow(dead_code)]
    token: String,
}

impl RevisionCloudService for GridRevisionCloudService {
    #[tracing::instrument(level = "trace", skip(self))]
    fn fetch_object(&self, _user_id: &str, _object_id: &str) -> FutureResult<Vec<Revision>, FlowyError> {
        FutureResult::new(async move { Ok(vec![]) })
    }
}

struct GridRevisionCompact();
impl RevisionCompact for GridRevisionCompact {
    fn compact_revisions(user_id: &str, object_id: &str, mut revisions: Vec<Revision>) -> FlowyResult<Revision> {
        if revisions.is_empty() {
            return Err(FlowyError::internal().context("Can't compact the empty folder's revisions"));
        }

        if revisions.len() == 1 {
            return Ok(revisions.pop().unwrap());
        }

        let first_revision = revisions.first().unwrap();
        let last_revision = revisions.last().unwrap();

        let (base_rev_id, rev_id) = first_revision.pair_rev_id();
        let md5 = last_revision.md5.clone();
        let delta = make_delta_from_revisions::<PlainTextAttributes>(revisions)?;
        let delta_data = delta.to_bytes();
        Ok(Revision::new(object_id, base_rev_id, rev_id, delta_data, user_id, md5))
    }
}
