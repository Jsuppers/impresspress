//! GET /b/auth/reset-password — relocated from auth/login.rs::handle_reset_password_form
//! in Task 5.

use maud::{html, PreEscaped};
use wafer_run::{context::Context, Message, OutputStream};

use crate::{
    ui,
    ui::{components::auth_panel, icons, templates::auth_split},
};

pub async fn handle(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let logo_url = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_LOGO_URL")
        .unwrap_or("")
        .to_string();
    let app_name = ctx
        .config_get("WAFER_RUN_SHARED__APP_NAME")
        .unwrap_or("Impresspress")
        .to_string();
    let auth_headline = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_HEADLINE")
        .unwrap_or(crate::config_vars::DEFAULT_AUTH_HEADLINE)
        .to_string();
    let auth_tagline = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_TAGLINE")
        .unwrap_or(crate::config_vars::DEFAULT_AUTH_TAGLINE)
        .to_string();

    let token = msg.get_meta("req.query.token").to_string();
    if token.is_empty() {
        return html_respond(
            "Invalid Link",
            "This password reset link is invalid.",
            false,
            &logo_url,
            &app_name,
            &auth_headline,
            &auth_tagline,
        );
    }

    let config = ui::SiteConfig {
        app_name: ctx
            .config_get("WAFER_RUN_SHARED__APP_NAME")
            .unwrap_or("Impresspress")
            .to_string(),
        logo_url,
        logo_icon_url: String::new(),
        favicon_url: crate::ui::assets::favicon_url(),
        primary_color: String::new(),
        embedded_scripts: Vec::new(),
        auth_headline,
        auth_tagline,
    };

    let markup = ui::layout::page(
        "Reset Password",
        &config,
        auth_split(
            auth_panel(&config, Some("Reset your password.")),
            html! {
                div .login-container {
                    div #error .login-error hidden {}
                    div #success .login-success hidden {}

                    form #form .login-form onsubmit="return handleReset(event)" {
                        input type="hidden" #reset-token name="token" value=(token);

                        div .form-group {
                            label .form-label for="password" { "New Password" }
                            input .form-input type="password" #password required minlength="8" placeholder="Min 8 characters";
                        }
                        div .form-group {
                            label .form-label for="confirm" { "Confirm Password" }
                            input .form-input type="password" #confirm required minlength="8" placeholder="Repeat password";
                        }

                        button .login-button type="submit" #btn { "Reset Password" }
                    }
                }

                script { (PreEscaped(r#"
var $=function(id){return document.getElementById(id)};
async function handleReset(e){
  e.preventDefault();
  var pw=$('password').value,cf=$('confirm').value;
  var err=$('error'),suc=$('success'),btn=$('btn');
  var token=$('reset-token').value;
  err.hidden=true;suc.hidden=true;
  if(pw!==cf){err.textContent='Passwords do not match.';err.hidden=false;return false;}
  if(pw.length<8){err.textContent='Password must be at least 8 characters.';err.hidden=false;return false;}
  btn.disabled=true;btn.textContent='Resetting...';
  try{
    var r=await fetch('/b/auth/api/reset-password',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token:token,new_password:pw})});
    var d=await r.json();
    if(d.error){err.textContent=d.error.message||d.error;err.hidden=false;}
    else{suc.textContent='Password reset successfully. You can now sign in.';suc.hidden=false;$('form').hidden=true;
      setTimeout(function(){window.location.href='/b/auth/login';},2000);}
  }catch(ex){err.textContent='Something went wrong.';err.hidden=false;}
  btn.disabled=false;btn.textContent='Reset Password';
  return false;
}
"#)) }
            },
        ),
    );

    ui::html_response(markup)
}

/// Return an HTML page response (for the invalid-token failure case).
fn html_respond(
    title: &str,
    message: &str,
    success: bool,
    logo_url: &str,
    app_name: &str,
    auth_headline: &str,
    auth_tagline: &str,
) -> OutputStream {
    // Static modifier rather than an inline `--icon-color`/`--icon-bg` pair:
    // the two states are fixed, so their colours belong in the stylesheet
    // where the contrast guard can see them (see auth-split.css).
    let icon_state = if success {
        "auth-status__icon--success"
    } else {
        "auth-status__icon--failure"
    };
    let config = ui::SiteConfig {
        app_name: app_name.to_string(),
        logo_url: logo_url.to_string(),
        logo_icon_url: String::new(),
        favicon_url: crate::ui::assets::favicon_url(),
        primary_color: String::new(),
        embedded_scripts: Vec::new(),
        auth_headline: auth_headline.to_string(),
        auth_tagline: auth_tagline.to_string(),
    };
    let markup = ui::layout::page(
        title,
        &config,
        auth_split(
            auth_panel(&config, Some("Reset your password.")),
            html! {
                div .login-container {
                    div .auth-status {
                        div class={"auth-status__icon " (icon_state)} aria-hidden="true" {
                            @if success { (icons::check()) } @else { (icons::x()) }
                        }
                        h2 .auth-status__title { (title) }
                        p .auth-status__message { (message) }
                        a .login-button .auth-status__action href="/b/auth/login" {
                            "Go to Sign In"
                        }
                    }
                }
            },
        ),
    );
    ui::html_response(markup)
}
