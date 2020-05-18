/*!
 * Library interfaces for this crate, intended for use only by the automated
 * test suite.  This crate does not define a Rust library API that's intended to
 * be consumed from the outside.
 *
 * TODO-cleanup is there a better way to do this?
 */

mod api_config;
mod api_error;
mod api_http_entrypoints;
pub mod api_model;
mod controller;
mod datastore;
mod server_controller;
mod test_util;

pub use api_config::ApiServerConfig;
pub use controller::OxideController;
pub use controller::OxideControllerTestInterfaces;
pub use server_controller::SimMode;
pub use server_controller::ServerControllerTestInterfaces;

use api_model::ApiIdentityMetadataCreateParams;
use api_model::ApiName;
use api_model::ApiProjectCreateParams;
use dropshot::ApiDescription;
use dropshot::RequestContext;
use server_controller::ServerController;
use std::any::Any;
use std::convert::TryFrom;
use std::sync::Arc;
use uuid::Uuid;

#[macro_use]
extern crate slog;

/**
 * Returns a Dropshot `ApiDescription` for our API.
 */
pub fn dropshot_api() -> ApiDescription {
    let mut api = ApiDescription::new();
    if let Err(err) = api_http_entrypoints::api_register_entrypoints(&mut api) {
        panic!("failed to register entrypoints: {}", err);
    }
    api
}

/**
 * Run the OpenAPI generator, which emits the OpenAPI spec to stdout.
 */
pub fn run_openapi() {
    dropshot_api().print_openapi();
}

/**
 * Run an instance of the API server.
 */
pub async fn run_server(config: &ApiServerConfig) -> Result<(), String> {
    let log = config
        .log
        .to_logger("oxide-controller")
        .map_err(|message| format!("initializing logger: {}", message))?;
    info!(log, "starting server");

    let dropshot_log = log.new(o!("component" => "dropshot"));
    let apictx = ApiContext::new(&Uuid::new_v4(), log);

    populate_initial_data(&apictx, SimMode::Auto).await;

    let mut http_server = dropshot::HttpServer::new(
        &config.dropshot,
        dropshot_api(),
        apictx,
        &dropshot_log,
    )
    .map_err(|error| format!("initializing server: {}", error))?;

    let join_handle = http_server.run().await;
    let server_result = join_handle
        .map_err(|error| format!("waiting for server: {}", error))?;
    server_result.map_err(|error| format!("server stopped: {}", error))
}

/**
 * API-specific state that we'll associate with the server and make available to
 * API request handler functions.
 */
pub struct ApiContext {
    pub controller: Arc<OxideController>,
    pub log: slog::Logger,
}

impl ApiContext {
    pub fn new(rack_id: &Uuid, log: slog::Logger) -> Arc<ApiContext> {
        Arc::new(ApiContext {
            controller: Arc::new(OxideController::new_with_id(
                rack_id,
                log.new(o!("component" => "controller")),
            )),
            log: log,
        })
    }

    /**
     * Retrieves our API-specific context out of the generic RequestContext
     * structure
     */
    pub fn from_request(rqctx: &Arc<RequestContext>) -> Arc<ApiContext> {
        Self::from_private(Arc::clone(&rqctx.server.private))
    }

    /**
     * Retrieves our API-specific context out of the generic HttpServer
     * structure.
     */
    pub fn from_server(server: &dropshot::HttpServer) -> Arc<ApiContext> {
        Self::from_private(server.app_private())
    }

    /**
     * Retrieves our API-specific context from the generic one stored in
     * Dropshot.
     */
    fn from_private(
        ctx: Arc<dyn Any + Send + Sync + 'static>,
    ) -> Arc<ApiContext> {
        /*
         * It should not be possible for this downcast to fail unless the caller
         * has passed us a RequestContext from a totally different HttpServer
         * or a totally different HttpServer itself (in either case created with
         * a different type for its private data).  This seems quite unlikely in
         * practice.
         * TODO-cleanup: can we make this API statically type-safe?
         */
        ctx.downcast::<ApiContext>()
            .expect("ApiContext: wrong type for private data")
    }
}

/*
 * This is a one-off for prepopulating some useful data in a freshly-started
 * server.  This should be replaced with a config file or a data backend with a
 * demo initialization script or the like.
 */
pub async fn populate_initial_data(
    apictx: &Arc<ApiContext>,
    sim_mode: SimMode,
) {
    let controller = &apictx.controller;
    let demo_projects: Vec<(&str, &str)> = vec![
        ("1eb2b543-b199-405f-b705-1739d01a197c", "simproject1"),
        ("4f57c123-3bda-4fae-94a2-46a9632d40b6", "simproject2"),
        ("4aac89b0-df9a-441d-b050-f953476ea290", "simproject3"),
    ];

    for (new_uuid, new_name) in demo_projects {
        let name_validated = ApiName::try_from(new_name).unwrap();
        controller
            .project_create_with_id(
                Uuid::parse_str(new_uuid).unwrap(),
                &ApiProjectCreateParams {
                    identity: ApiIdentityMetadataCreateParams {
                        name: name_validated,
                        description: "<auto-generated at server startup>"
                            .to_string(),
                    },
                },
            )
            .await
            .unwrap();
    }

    let demo_controllers = vec![
        "b6d65341-167c-41df-9b5c-41cded99c229",
        "2335aceb-969e-4abc-bbba-b0d3b44bc82e",
        "dae9faf7-5b13-4334-85ed-6a53d0835414",
    ];
    for uuidstr in demo_controllers {
        let uuid = Uuid::parse_str(uuidstr).unwrap();
        let sc = ServerController::new_simulated_with_id(
            &uuid,
            sim_mode,
            apictx.log.new(o!("server_controller" => uuid.to_string())),
            controller.as_sc_api(),
        );
        controller.add_server_controller(Arc::new(sc)).await;
    }
}
