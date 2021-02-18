#[macro_use]
extern crate diesel;
mod db;
mod requests;
use std::num::ParseIntError;

use diesel::{prelude::*, result::DatabaseErrorKind};

use db::models::{NewLoyalty, NewUser};
use diesel::RunQueryDsl;
use requests::{AddLoyalty, AddLoyaltyResponse, UserSignIn, UserSignup};

use rocket::http::Cookie;
use rocket::{
    delete, get,
    http::Status,
    launch, post, put,
    request::Outcome,
    response::{status, Responder},
    routes, Response,
};
use rocket::{http::CookieJar, request::FromRequest};
use rocket_contrib::{database, json::Json};
use thiserror::Error;
use validator::{Validate, ValidationErrors};

#[derive(Debug, Error)]
enum APIError {
    #[error("error during sign in")]
    SignError(#[from] ValidationErrors),
    #[error("query error")]
    DieselError(#[from] diesel::result::Error),
    #[error("not authorised")]
    NotAuthorized,
    #[error("parsing error")]
    ParsingError(#[from] ParseIntError),
}

impl<'a> Responder<'a, 'static> for APIError {
    fn respond_to(self, _request: &rocket::Request<'_>) -> rocket::response::Result<'static> {
        let mut resp = Response::build();

        let status = match self {
            APIError::SignError(..) => Status::BadRequest,
            APIError::DieselError(ref e) => match e {
                diesel::result::Error::DatabaseError(kind, ..)
                    if matches!(kind, DatabaseErrorKind::UniqueViolation) =>
                {
                    Status::BadRequest
                }
                _ => Status::InternalServerError,
            },
            APIError::ParsingError(..) => Status::BadRequest,
            _ => Status::InternalServerError,
        };

        resp.status(status).ok()
    }
}

#[database("loyalty_db")]
struct LoyaltyDbConn(diesel::SqliteConnection);

#[launch]
fn rocket() -> rocket::Rocket {
    rocket::ignite().attach(LoyaltyDbConn::fairing()).mount(
        "/",
        routes![
            signup,
            signin,
            get_user,
            sign_out,
            add_loyalty,
            get_loyalties,
            delete_loyalty
        ],
    )
}

#[post("/signup", format = "json", data = "<body>")]
async fn signup(db: LoyaltyDbConn, body: Json<UserSignup>) -> Result<(), APIError> {
    body.0.validate()?;

    db.run(move |c| {
        let new_value = NewUser {
            email: &body.0.email,
            name: &body.0.name,
            pass: &body.0.pass,
        };

        diesel::insert_into(db::schema::users::table)
            .values(&new_value)
            .execute(c)?;

        Ok(())
    })
    .await
}

#[post("/signin", format = "json", data = "<body>")]
async fn signin(
    cookies: &CookieJar<'_>,
    db: LoyaltyDbConn,
    body: Json<UserSignIn>,
) -> Result<status::Custom<&'static str>, APIError> {
    use db::schema::users::dsl::*;

    let fetched = db
        .run(move |c| {
            let req = body.0;

            users
                .filter(email.eq(req.email).and(pass.eq(req.pass)))
                .limit(1)
                .load::<db::models::User>(c)
        })
        .await?;

    if fetched.is_empty() {
        Ok(status::Custom(Status::Forbidden, "invalid credentials"))
    } else {
        let user = &fetched[0];
        cookies.add_private(Cookie::new("user_id", user.id.to_string()));
        Ok(status::Custom(Status::Ok, "connected"))
    }
}

#[post("/signout")]
async fn sign_out(cookies: &CookieJar<'_>) -> status::Custom<&'static str> {
    cookies.remove_private(Cookie::named("user_id"));
    status::Custom(Status::Ok, "logged out")
}

#[derive(Debug)]
struct User(i32);

use rocket::async_trait;

#[crate::async_trait]
impl<'a, 'r> FromRequest<'a, 'r> for User {
    type Error = APIError;

    async fn from_request(
        request: &'a rocket::Request<'r>,
    ) -> rocket::request::Outcome<Self, Self::Error> {
        if let Some(user) = request
            .cookies()
            .get_private("user_id")
            .and_then(|c| c.value().parse().ok())
            .map(|id| User(id))
        {
            Outcome::Success(user)
        } else {
            Outcome::Failure((Status::Forbidden, APIError::NotAuthorized))
        }
    }
}

#[get("/userinfo")]
async fn get_user(db: LoyaltyDbConn, user: User) -> Option<Json<db::models::User>> {
    use db::schema::users::dsl::*;
    let fetched = db
        .run(move |c| {
            users
                .filter(id.eq(user.0))
                .limit(1)
                .load::<db::models::User>(c)
        })
        .await;

    if fetched.is_err() {
        return None;
    }

    let mut elements: Vec<db::models::User> = fetched.unwrap();

    if elements.is_empty() {
        None
    } else {
        let found = elements.remove(0);
        Some(Json(found))
    }
}

#[put("/loyalties", format = "json", data = "<body>")]
async fn add_loyalty(
    db: LoyaltyDbConn,
    user: User,
    body: Json<AddLoyalty>,
) -> Option<Json<AddLoyaltyResponse>> {
    use db::schema::cards::dsl::*;

    let mut last_inserted = db
        .run(move |c| {
            let new_value = NewLoyalty {
                name: &body.0.name,
                color: body.0.color.as_deref(),
                code: &body.0.code,
                user_id: user.0,
            };

            diesel::insert_into(db::schema::cards::table)
                .values(&new_value)
                .execute(c)
                .ok()?;

            Ok(cards
                .order(id.desc())
                .limit(1)
                .load::<db::models::Loyalty>(c)
                .ok()?)
        })
        .await?;

    let last = last_inserted.remove(0);
    Some(Json(AddLoyaltyResponse {
        id: last.id.to_string(),
        name: last.name,
        color: last.color,
        code: last.code,
    }))
}

#[get("/loyalties?<limit>&<offset>")]
async fn get_loyalties(
    db: LoyaltyDbConn,
    user: User,
    limit: Option<String>,
    offset: Option<String>,
) -> Option<Json<Vec<AddLoyaltyResponse>>> {
    use db::schema::cards::dsl::*;

    let limit = limit.and_then(|p| p.parse().ok()).unwrap_or(10);
    let offset = offset.and_then(|p| p.parse().ok()).unwrap_or(0);

    let elements = db
        .run(move |c| {
            cards
                .filter(user_id.eq(user.0))
                .limit(limit)
                .offset(offset)
                .load::<db::models::Loyalty>(c)
                .ok()
        })
        .await?;

    let new: Vec<_> = elements
        .into_iter()
        .map(|last| AddLoyaltyResponse {
            id: last.id.to_string(),
            name: last.name,
            color: last.color,
            code: last.code,
        })
        .collect();
    Some(Json(new))
}

#[delete("/loyalties/<loyalty_id>")]
async fn delete_loyalty(
    db: LoyaltyDbConn,
    loyalty_id: String,
) -> Result<status::Custom<&'static str>, APIError> {
    use db::schema::cards::dsl::*;

    let loyalty_id: i32 = loyalty_id.parse()?;

    db.run(move |c| diesel::delete(cards.filter(id.eq(loyalty_id))).execute(c))
        .await?;
    Ok(status::Custom(Status::Ok, "loyalty deleted"))
}
