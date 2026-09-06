//! Admin JSON API handlers.

use serde::de::DeserializeOwned;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::{
    contracts::{
        AddNoteRequest, AdminCreateTicketRequest, AnalysisListResponse, TicketAnalysisView,
        TicketDetailResponse, TicketEventView, TicketListQuery, TicketListResponse,
        TicketTypeListQuery, TicketTypeListResponse, TicketTypeView, TicketView,
    },
    maintenance,
    models::{
        ActorType, AnalysisInput, CreateTicketInput, Priority, TicketSource, TicketStatus,
        TicketTypeInput, TicketTypeUpdate, WorkflowUpdate,
    },
    repo::{self, TicketFilters},
    service,
};
use crate::{
    blocks::crud,
    http::{err_bad_request, err_conflict, err_internal, err_not_found, ok_json, ResponseBuilder},
};

const MAX_ADMIN_BODY: usize = 32 * 1_024;

pub async fn list_tickets(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = TicketListQuery::from_message(msg);
    if query.status.as_deref().is_some_and(invalid::<TicketStatus>) {
        return err_bad_request("Invalid status filter");
    }
    if query.priority.as_deref().is_some_and(invalid::<Priority>) {
        return err_bad_request("Invalid priority filter");
    }
    if query.source.as_deref().is_some_and(invalid::<TicketSource>) {
        return err_bad_request("Invalid source filter");
    }
    let offset = i64::from(query.page.saturating_sub(1)) * i64::from(query.page_size);
    let filters = TicketFilters {
        status: query.status.as_deref(),
        priority: query.priority.as_deref(),
        type_id: query.type_id.as_deref(),
        source: query.source.as_deref(),
        assignee_id: query.assignee_id.as_deref(),
    };
    match repo::list_tickets(ctx, &filters, i64::from(query.page_size), offset).await {
        Ok(rows) => ok_json(&TicketListResponse::from_record_list(&rows)),
        Err(error) => err_internal("Could not list tickets", error),
    }
}

pub async fn create_ticket(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let body: AdminCreateTicketRequest = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let source: TicketSource = match body.source.parse() {
        Ok(TicketSource::PublicForm) | Err(_) => {
            return err_bad_request("Admin creation source must be admin, api, or ai")
        }
        Ok(source) => source,
    };
    let actor = match source {
        TicketSource::Admin => ActorType::Admin,
        TicketSource::Api => ActorType::Api,
        TicketSource::Ai => ActorType::Ai,
        TicketSource::PublicForm => unreachable!(),
    };
    let ticket = CreateTicketInput {
        type_id: body.type_id,
        subject: body.subject,
        description: body.description,
        source_path: body.source_path,
        subject_type: body.subject_type,
        subject_id: body.subject_id,
        evidence_url: body.evidence_url,
        reporter_email: String::new(),
        reporter_wants_reply: false,
        priority: body.priority,
    };
    match service::create_ticket(ctx, ticket, source, actor, msg.user_id(), None).await {
        Ok(record) => ResponseBuilder::new()
            .status(201)
            .json(&TicketView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn get_ticket(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, "Ticket") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::detail(ctx, id).await {
        Ok(detail) => ok_json(&TicketDetailResponse::from_detail(detail)),
        Err(service::ServiceError::Db(error)) if error.code == wafer_run::ErrorCode::NotFound => {
            err_not_found("Ticket not found")
        }
        Err(error) => service_error(error),
    }
}

pub async fn update_ticket(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let body: WorkflowUpdate = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::update_workflow(ctx, msg.var("id"), body, ActorType::Admin, msg.user_id()).await
    {
        Ok(record) => ok_json(&TicketView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn add_note(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let body: AddNoteRequest = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::add_note(
        ctx,
        msg.var("id"),
        &body.note,
        ActorType::Admin,
        msg.user_id(),
    )
    .await
    {
        Ok(record) => ResponseBuilder::new()
            .status(201)
            .json(&TicketEventView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn list_analyses(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match repo::list_analyses(ctx, msg.var("id"), 100).await {
        Ok(records) => ok_json(&AnalysisListResponse::from_records(&records)),
        Err(error) => err_internal("Could not list analyses", error),
    }
}

pub async fn add_analysis(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let body: AnalysisInput = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::add_analysis(ctx, msg.var("id"), body).await {
        Ok(record) => ResponseBuilder::new()
            .status(201)
            .json(&TicketAnalysisView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn list_types(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = TicketTypeListQuery::from_message(msg);
    let offset = i64::from(query.page.saturating_sub(1)) * i64::from(query.page_size);
    match repo::list_types(ctx, false, i64::from(query.page_size), offset).await {
        Ok(rows) => ok_json(&TicketTypeListResponse::from_record_list(&rows)),
        Err(error) => err_internal("Could not list ticket types", error),
    }
}

pub async fn create_type(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let body: TicketTypeInput = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::create_type(ctx, body).await {
        Ok(record) => ResponseBuilder::new()
            .status(201)
            .json(&TicketTypeView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn update_type(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let body: TicketTypeUpdate = match collect_json(input).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match service::update_type(ctx, msg.var("id"), body).await {
        Ok(record) => ok_json(&TicketTypeView::from_record(&record)),
        Err(error) => service_error(error),
    }
}

pub async fn status(ctx: &dyn Context) -> OutputStream {
    match maintenance::status(ctx).await {
        Ok(status) => ok_json(&status),
        Err(error) => err_internal("Could not load ticket status", error),
    }
}

pub async fn prune(ctx: &dyn Context) -> OutputStream {
    let result = maintenance::prune(ctx).await;
    if result.complete {
        ok_json(&result)
    } else {
        ResponseBuilder::new().status(503).json(&result)
    }
}

async fn collect_json<T: DeserializeOwned>(input: InputStream) -> Result<T, OutputStream> {
    let raw = input.collect_to_bytes().await;
    if raw.len() > MAX_ADMIN_BODY {
        return Err(ResponseBuilder::new()
            .status(413)
            .body(b"Request is too large".to_vec(), "text/plain"));
    }
    serde_json::from_slice(&raw)
        .map_err(|error| err_bad_request(&format!("Invalid JSON request: {error}")))
}

fn service_error(error: service::ServiceError) -> OutputStream {
    match error {
        service::ServiceError::Validation(message) => err_bad_request(&message),
        service::ServiceError::Conflict(message) => err_conflict(&message),
        service::ServiceError::Db(error) if error.code == wafer_run::ErrorCode::NotFound => {
            err_not_found("Resource not found")
        }
        service::ServiceError::Db(error) => err_internal("Database error", error),
    }
}

/// Whether a filter value fails to parse as `T`, which is answered with 400.
fn invalid<T: std::str::FromStr>(value: &str) -> bool {
    value.parse::<T>().is_err()
}
