//! Loco requires a SeaORM migrator. Ferrum's tables are created by the kernel (SQL migrations +
//! metadata-driven DDL) at boot, so this migrator is intentionally empty.
pub use sea_orm_migration::prelude::*;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![]
    }
}
