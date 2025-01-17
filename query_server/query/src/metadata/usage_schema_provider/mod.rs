use std::collections::HashMap;
use std::sync::Arc;

use datafusion::datasource::{provider_as_source, TableProvider, ViewTable};
use datafusion::logical_expr::{binary_expr, col, LogicalPlanBuilder, Operator};
use datafusion::prelude::lit;
use meta::error::MetaError;
use models::schema::DEFAULT_CATALOG;
use spi::query::session::SessionCtx;
use spi::{QueryError, Result};

use super::TableHandleProviderRef;
use crate::data_source::table_source::TableHandle;

pub const USAGE_SCHEMA: &str = "usage_schema";

pub struct UsageSchemaProvider {
    table_factories: HashMap<String, BoxUsageSchemaTableFactory>,
    default_table_provider: TableHandleProviderRef,
}

impl UsageSchemaProvider {
    pub fn new(default_table_provider: TableHandleProviderRef) -> Self {
        let mut provider = Self {
            table_factories: Default::default(),
            default_table_provider,
        };
        use crate::generate_usage_schema_table_factory;
        macro_rules! register_table_factory {
            ($measure: expr, $STRUCT_NAME:ident) => {
                generate_usage_schema_table_factory!($measure, $STRUCT_NAME);
                provider.register_table_factory(Box::new($STRUCT_NAME {}));
            };
        }
        register_table_factory!("coord_data_in", CoordDataIn);
        register_table_factory!("sql_data_in", SQLDataIn);
        register_table_factory!("write_data_in", WriteDataIn);
        register_table_factory!("sql_write_row", SQLWriteRow);
        register_table_factory!("sql_points_data_in", SQLPointsDataIn);
        register_table_factory!("coord_data_out", CoordDataOut);
        register_table_factory!("http_data_out", HttpDataOut);
        register_table_factory!("user_queries", UserQueries);
        register_table_factory!("user_writes", UserWrites);
        register_table_factory!("vnode_cache_size", VnodeCacheSize);
        register_table_factory!("vnode_disk_storage", VnodeDiskStorage);
        provider
    }

    fn register_table_factory(&mut self, factory: BoxUsageSchemaTableFactory) {
        let _ = self
            .table_factories
            .insert(factory.table_name().to_ascii_lowercase(), factory);
    }

    pub fn name(&self) -> &str {
        USAGE_SCHEMA
    }

    pub fn table(&self, session: &SessionCtx, name: &str) -> Result<Arc<dyn TableProvider>> {
        let usage_schema_table = self
            .table_factories
            .get(name)
            .ok_or_else(|| MetaError::TableNotFound { table: name.into() })?;
        usage_schema_table.create(session, &self.default_table_provider)
    }
}

type BoxUsageSchemaTableFactory = Box<dyn UsageSchemaTableFactory + Send + Sync>;

pub trait UsageSchemaTableFactory {
    fn table_name(&self) -> &str;
    fn create(
        &self,
        session: &SessionCtx,
        base_table_provider: &TableHandleProviderRef,
    ) -> Result<Arc<dyn TableProvider>>;
}

pub fn create_usage_schema_view_table(
    session: &SessionCtx,
    default_table_provider: &TableHandleProviderRef,
    view_table_name: &str,
) -> spi::Result<Arc<dyn TableProvider>> {
    let tenant_name = session.tenant();
    let table_handle = default_table_provider.build_table_handle(USAGE_SCHEMA, view_table_name)?;

    let table_source = match table_handle {
        TableHandle::Tskv(table_provider) => provider_as_source(table_provider),
        other => {
            return Err(QueryError::Internal {
                reason: format!("Usage schema data source is tskv, but found: {}", other),
            });
        }
    };

    let builder = LogicalPlanBuilder::scan(view_table_name.to_string(), table_source, None)?;

    let builder = if session.user().desc().is_admin() && tenant_name.eq(DEFAULT_CATALOG) {
        // do nothing
        builder
    } else {
        builder.filter(binary_expr(col("tenant"), Operator::Eq, lit(tenant_name)))?
    };

    let logical_plan = builder.build()?;

    Ok(Arc::new(ViewTable::try_new(logical_plan, None)?))
}

#[macro_export]
macro_rules! generate_usage_schema_table_factory {
    ($measure: expr, $STRUCT_NAME: ident) => {
        struct $STRUCT_NAME {}
        impl UsageSchemaTableFactory for $STRUCT_NAME {
            fn table_name(&self) -> &str {
                $measure
            }
            fn create(
                &self,
                session: &SessionCtx,
                base_table_provider: &TableHandleProviderRef,
            ) -> Result<Arc<dyn TableProvider>> {
                create_usage_schema_view_table(session, base_table_provider, $measure)
            }
        }
    };
}
