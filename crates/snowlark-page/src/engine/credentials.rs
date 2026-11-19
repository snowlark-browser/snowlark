// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential Management Level 1 (`navigator.credentials`) with its credential types: password and
//! federated credentials over a credential store that stays empty (the engine keeps no passwords and
//! is never granted "permission to store"), WebAuthn's `PublicKeyCredential` with no authenticator
//! and FedCM's `IdentityCredential` with no identity provider.

use snowlark_bindings::generated::{
    AllAcceptedCredentialsOptions, AuthenticationExtensionsClientOutputs,
    AuthenticatorAssertionResponseImpl, AuthenticatorAttestationResponseImpl,
    AuthenticatorResponseImpl, CredentialCreationOptions, CredentialImpl,
    CredentialMediationRequirement, CredentialRequestOptions, CredentialUserDataImpl,
    CredentialsContainerImpl, CurrentUserDetailsOptions, FederatedCredentialImpl,
    FederatedCredentialInit, IdentityCredentialDisconnectOptions, IdentityCredentialImpl,
    PasswordCredentialData, PasswordCredentialDataOrHTMLFormElement, PasswordCredentialImpl,
    PublicKeyCredentialCreationOptions, PublicKeyCredentialCreationOptionsJSON,
    PublicKeyCredentialImpl, PublicKeyCredentialRequestOptions,
    PublicKeyCredentialRequestOptionsJSON, UnknownCredentialOptions,
};
use snowlark_bindings::rt::{
    self, Cx, DomStr, Error, ExceptionName, Handle, JsObject, JsPromise, JsValue,
};
use snowlark_dom::NodeId;

use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// A credential object's data (Credential Management §2.2 and the types' own slots).
pub struct CredentialData {
    pub id: String,
    /// The [[origin]] slot (serialized).
    pub origin: String,
    pub kind: CredentialKind,
}

pub enum CredentialKind {
    Password {
        password: String,
        name: String,
        icon_url: String,
    },
    Federated {
        provider: String,
        protocol: Option<String>,
        name: String,
        icon_url: String,
    },
}

/// The credential types `get()`/`create()` options can name (their options member identifiers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Type {
    Password,
    Federated,
    PublicKey,
    Identity,
    /// A type whose interface this engine does not expose (`otp`, `digital`).
    Unsupported,
}

impl Type {
    /// The registry's "types allowed in the same get() request": only password and federated go
    /// together.
    fn allowed_with(self, other: Type) -> bool {
        matches!(
            (self, other),
            (Type::Password, Type::Federated) | (Type::Federated, Type::Password)
        )
    }
}

/// How a credentials container promise settles.
enum Outcome {
    Credential(Handle),
    Null,
    Undefined,
}

impl Engine {
    fn credential<R>(&self, h: Handle, f: impl FnOnce(&CredentialData) -> R) -> Option<R> {
        match &*self.obj(h) {
            Object::Credential(c) => Some(f(c)),
            _ => None,
        }
    }

    /// "fully active" for the current settings object's document.
    fn credentials_document(&self, cx: &mut Cx) -> Option<NodeId> {
        let doc = self.current_document(cx).ok()?;
        self.is_active_document(doc).then_some(doc)
    }

    /// "same-origin with its ancestors".
    fn same_origin_with_ancestors(&self, doc: NodeId) -> bool {
        let origin = self.origin_of_document(doc);
        let mut current = doc;
        while let Some(parent) = self.parent_document(current) {
            if self.origin_of_document(parent) != origin {
                return false;
            }
            current = parent;
        }
        true
    }

    fn origin_is_opaque(&self, doc: NodeId) -> bool {
        let o = self.origin_of_document(doc);
        o == "null" || o.starts_with("opaque:")
    }

    /// Settles `p` in a task (the algorithms' "in parallel" parts): with the abort reason when
    /// `signal` aborted meanwhile, else with `settle`'s outcome.
    fn settle_credential_promise(
        &self,
        p: &JsPromise,
        signal: Option<Handle>,
        settle: impl FnOnce(&Engine, &mut Cx) -> rt::Result<Outcome> + 'static,
    ) {
        let p = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if let Some(s) = signal.filter(|&s| e.signal_aborted(s)) {
                if let Some(reason) = e.signal_reason(cx, s) {
                    p.reject_value(cx, &reason);
                    return;
                }
            }
            match settle(e, cx) {
                Ok(Outcome::Credential(c)) => p.resolve(cx, c),
                Ok(Outcome::Null) => p.resolve(cx, Option::<Handle>::None),
                Ok(Outcome::Undefined) => p.resolve(cx, ()),
                Err(err) => p.reject(cx, err),
            }
        });
    }

    /// The promise rejected now with an aborted signal's reason, when it is aborted.
    fn rejected_if_aborted(&self, cx: &mut Cx, signal: Option<Handle>) -> Option<JsPromise> {
        let s = signal.filter(|&s| self.signal_aborted(s))?;
        let reason = self.signal_reason(cx, s)?;
        let p = JsPromise::new(cx)?;
        p.reject_value(cx, &reason);
        Some(p)
    }

    fn rejected_promise(&self, cx: &mut Cx, err: Error) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        p.reject(cx, err);
        Ok(p)
    }

    /// §2.5.1 "Request a Credential".
    fn request_credential(
        &self,
        cx: &mut Cx,
        options: CredentialRequestOptions,
    ) -> rt::Result<JsPromise> {
        let Some(doc) = self.credentials_document(cx) else {
            return self.rejected_promise(
                cx,
                exception(
                    ExceptionName::InvalidStateError,
                    "The document is not fully active",
                ),
            );
        };
        if let Some(p) = self.rejected_if_aborted(cx, options.signal) {
            return Ok(p);
        }
        // The options' relevant credential interface objects.
        let mut types = Vec::new();
        if options.password {
            types.push(Type::Password);
        }
        if options.federated.is_some() {
            types.push(Type::Federated);
        }
        if options.public_key.is_some() {
            types.push(Type::PublicKey);
        }
        if options.identity.is_some() {
            types.push(Type::Identity);
        }
        if options.otp.is_some() || options.digital.is_some() {
            types.push(Type::Unsupported);
        }
        let mediation = options.mediation;
        let same_origin = self.same_origin_with_ancestors(doc);
        let opaque = self.origin_is_opaque(doc);
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        self.settle_credential_promise(&p, options.signal, move |_, _| {
            if types.is_empty() || types.contains(&Type::Unsupported) {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "No supported credential type was requested",
                ));
            }
            for (i, a) in types.iter().enumerate() {
                if types[i + 1..].iter().any(|b| !a.allowed_with(*b)) {
                    return Err(exception(
                        ExceptionName::NotSupportedError,
                        "These credential types cannot be requested together",
                    ));
                }
            }
            // (every type here is origin-bound)
            if opaque {
                return Err(exception(
                    ExceptionName::SecurityError,
                    "An opaque origin has no credentials",
                ));
            }
            if mediation == CredentialMediationRequirement::Conditional
                && types.iter().any(|t| *t != Type::PublicKey)
            {
                return Err(Error::Type(
                    "This credential type does not support conditional mediation".into(),
                ));
            }
            for t in &types {
                match t {
                    // [[CollectFromCredentialStore]]: same-origin with its ancestors, else
                    // NotAllowedError; the store has nothing.
                    Type::Password | Type::Federated if !same_origin => {
                        return Err(exception(
                            ExceptionName::NotAllowedError,
                            "Credentials are only available to documents same origin with their ancestors",
                        ))
                    }
                    // WebAuthn with no authenticator: the ceremony ends as a cancelled one.
                    Type::PublicKey => {
                        return Err(exception(
                            ExceptionName::NotAllowedError,
                            "No authenticator is available",
                        ))
                    }
                    // FedCM with no identity provider reachable.
                    Type::Identity => {
                        return Err(exception(
                            ExceptionName::NetworkError,
                            "No identity provider is available",
                        ))
                    }
                    _ => {}
                }
            }
            // No credential collected: silent mediation resolves null; otherwise the user would
            // choose among none, which is null as well (a conditional request never settles).
            if mediation == CredentialMediationRequirement::Conditional {
                return Err(Error::Pending);
            }
            Ok(Outcome::Null)
        });
        Ok(p)
    }

    /// §2.5.3 "Store a Credential".
    fn store_credential(&self, cx: &mut Cx, credential: Handle) -> rt::Result<JsPromise> {
        let Some(doc) = self.credentials_document(cx) else {
            return self.rejected_promise(
                cx,
                exception(
                    ExceptionName::InvalidStateError,
                    "The document is not fully active",
                ),
            );
        };
        if self.origin_is_opaque(doc) {
            return self.rejected_promise(
                cx,
                exception(
                    ExceptionName::SecurityError,
                    "An opaque origin cannot store credentials",
                ),
            );
        }
        let known = self.credential(credential, |_| ()).is_some();
        let same_origin = self.same_origin_with_ancestors(doc);
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        self.settle_credential_promise(&p, None, move |_, _| {
            if !known {
                // Credential's default [[Store]].
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "This credential type cannot be stored",
                ));
            }
            if !same_origin {
                return Err(exception(
                    ExceptionName::NotAllowedError,
                    "Credentials are only stored for documents same origin with their ancestors",
                ));
            }
            // The user agent has no "permission to store": the store does not change.
            Ok(Outcome::Undefined)
        });
        Ok(p)
    }

    /// §2.5.4 "Create a Credential".
    fn create_credential(
        &self,
        cx: &mut Cx,
        options: CredentialCreationOptions,
    ) -> rt::Result<JsPromise> {
        let Some(doc) = self.credentials_document(cx) else {
            return self.rejected_promise(
                cx,
                exception(
                    ExceptionName::InvalidStateError,
                    "The document is not fully active",
                ),
            );
        };
        if let Some(p) = self.rejected_if_aborted(cx, options.signal) {
            return Ok(p);
        }
        let origin = self.origin_of_document(doc);
        let opaque = self.origin_is_opaque(doc);
        let count = options.password.is_some() as usize
            + options.federated.is_some() as usize
            + options.public_key.is_some() as usize
            + options.digital.is_some() as usize;
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let CredentialCreationOptions {
            password,
            federated,
            public_key,
            signal,
            ..
        } = options;
        self.settle_credential_promise(&p, signal, move |e, _| {
            if count != 1 || (password.is_none() && federated.is_none() && public_key.is_none()) {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "Exactly one supported credential type must be given",
                ));
            }
            if opaque {
                return Err(exception(
                    ExceptionName::SecurityError,
                    "An opaque origin has no credentials",
                ));
            }
            if let Some(init) = password {
                let data = match init {
                    PasswordCredentialDataOrHTMLFormElement::PasswordCredentialData(d) => d,
                    PasswordCredentialDataOrHTMLFormElement::HTMLFormElement(form) => {
                        e.password_data_from_form(form, &origin)?
                    }
                };
                return e
                    .password_credential_from_data(data)
                    .map(Outcome::Credential);
            }
            if let Some(init) = federated {
                return e
                    .federated_credential_from_init(FederatedCredentialInit {
                        origin: origin.clone(),
                        ..init
                    })
                    .map(Outcome::Credential);
            }
            Err(exception(
                ExceptionName::NotAllowedError,
                "No authenticator is available",
            ))
        });
        Ok(p)
    }

    /// "Create a PasswordCredential from PasswordCredentialData".
    fn password_credential_from_data(&self, data: PasswordCredentialData) -> rt::Result<Handle> {
        if data.id.is_empty() || data.origin.is_empty() || data.password.is_empty() {
            return Err(Error::Type(
                "A password credential needs an id, an origin and a password".into(),
            ));
        }
        Ok(self.alloc(Object::Credential(Box::new(CredentialData {
            id: data.id,
            origin: data.origin,
            kind: CredentialKind::Password {
                password: data.password,
                name: data.name.unwrap_or_default(),
                icon_url: data.icon_url.unwrap_or_default(),
            },
        }))))
    }

    /// "Create a PasswordCredential from an HTMLFormElement": the form's entries picked by their
    /// controls' `autocomplete` tokens.
    fn password_data_from_form(
        &self,
        form: Handle,
        origin: &str,
    ) -> rt::Result<PasswordCredentialData> {
        let form = super::handle_node(form).ok_or(Error::Pending)?;
        let entries = self.form_entries(form, None, encoding_rs::UTF_8);
        let controls = {
            let h = self.collection(form, super::collections::Filter::FormElements(form));
            let n = self.coll_len(h);
            (0..n)
                .filter_map(|i| self.coll_item(h, i).and_then(super::handle_node))
                .collect::<Vec<_>>()
        };
        let mut data = PasswordCredentialData {
            id: String::new(),
            icon_url: None,
            name: None,
            origin: origin.to_owned(),
            password: String::new(),
        };
        let mut new_password = false;
        for c in controls {
            let (autocomplete, name) = {
                let dom = self.dom();
                (
                    dom.get_attribute(c, "autocomplete")
                        .map(str::to_ascii_lowercase),
                    dom.get_attribute(c, "name").map(str::to_owned),
                )
            };
            let (Some(autocomplete), Some(name)) = (autocomplete, name) else {
                continue;
            };
            let Some(value) = entries
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, v)| match v {
                    super::fetch::FormValue::Str(v) => Some(v.clone()),
                    super::fetch::FormValue::File(_) => None,
                })
            else {
                continue;
            };
            for token in autocomplete.split_ascii_whitespace() {
                match token {
                    "new-password" => {
                        data.password = value.clone();
                        new_password = true;
                    }
                    "current-password" if !new_password => data.password = value.clone(),
                    "photo" => data.icon_url = Some(value.clone()),
                    "name" | "nickname" => data.name = Some(value.clone()),
                    "username" => data.id = value.clone(),
                    _ => {}
                }
            }
        }
        Ok(data)
    }

    /// "Create a FederatedCredential from FederatedCredentialInit".
    fn federated_credential_from_init(&self, init: FederatedCredentialInit) -> rt::Result<Handle> {
        if init.id.is_empty() || init.provider.is_empty() {
            return Err(Error::Type(
                "A federated credential needs an id and a provider".into(),
            ));
        }
        Ok(self.alloc(Object::Credential(Box::new(CredentialData {
            id: init.id,
            origin: init.origin,
            kind: CredentialKind::Federated {
                provider: init.provider,
                protocol: init.protocol,
                name: init.name.unwrap_or_default(),
                icon_url: init.icon_url.unwrap_or_default(),
            },
        }))))
    }
}

impl CredentialsContainerImpl for Engine {
    fn get(
        &self,
        cx: &mut Cx,
        _this: Handle,
        options: CredentialRequestOptions,
    ) -> rt::Result<JsPromise> {
        self.request_credential(cx, options)
    }
    fn store(&self, cx: &mut Cx, _this: Handle, credential: Handle) -> rt::Result<JsPromise> {
        self.store_credential(cx, credential)
    }
    fn create(
        &self,
        cx: &mut Cx,
        _this: Handle,
        options: CredentialCreationOptions,
    ) -> rt::Result<JsPromise> {
        self.create_credential(cx, options)
    }
    /// §2.5.5 "Prevent Silent Access": the origin's flag in a store that keeps nothing.
    fn prevent_silent_access(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        if self.credentials_document(cx).is_none() {
            return self.rejected_promise(
                cx,
                exception(
                    ExceptionName::InvalidStateError,
                    "The document is not fully active",
                ),
            );
        }
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        self.settle_credential_promise(&p, None, |_, _| Ok(Outcome::Undefined));
        Ok(p)
    }
}

impl CredentialImpl for Engine {
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.credential(this, |c| c.id.clone()).unwrap_or_default())
    }
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(
            self.credential(this, |c| match c.kind {
                CredentialKind::Password { .. } => "password",
                CredentialKind::Federated { .. } => "federated",
            })
            .unwrap_or(""),
        )
    }
    /// No credential type here supports conditional mediation.
    fn static_is_conditional_mediation_available(&self, cx: &mut Cx) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        p.resolve(cx, false);
        Ok(p)
    }
}

impl CredentialUserDataImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.credential(this, |c| match &c.kind {
                CredentialKind::Password { name, .. } | CredentialKind::Federated { name, .. } => {
                    name.clone()
                }
            })
            .unwrap_or_default(),
        )
    }
    fn icon_url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.credential(this, |c| match &c.kind {
                CredentialKind::Password { icon_url, .. }
                | CredentialKind::Federated { icon_url, .. } => icon_url.clone(),
            })
            .unwrap_or_default(),
        )
    }
}

impl PasswordCredentialImpl for Engine {
    fn password(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.credential(this, |c| match &c.kind {
                CredentialKind::Password { password, .. } => password.clone(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        )
    }
    fn constructor_form(&self, cx: &mut Cx, form: Handle) -> rt::Result<Handle> {
        let doc = self.current_document(cx)?;
        let origin = self.origin_of_document(doc);
        let data = self.password_data_from_form(form, &origin)?;
        self.password_credential_from_data(data)
    }
    fn constructor_data(&self, _cx: &mut Cx, data: PasswordCredentialData) -> rt::Result<Handle> {
        self.password_credential_from_data(data)
    }
}

impl FederatedCredentialImpl for Engine {
    fn provider(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.credential(this, |c| match &c.kind {
                CredentialKind::Federated { provider, .. } => provider.clone(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        )
    }
    fn protocol(&self, this: Handle) -> Option<DomStr<'_>> {
        self.credential(this, |c| match &c.kind {
            CredentialKind::Federated { protocol, .. } => protocol.clone(),
            _ => None,
        })
        .flatten()
        .map(DomStr::Owned)
    }
    fn constructor(&self, _cx: &mut Cx, data: FederatedCredentialInit) -> rt::Result<Handle> {
        self.federated_credential_from_init(data)
    }
}

/// WebAuthn without an authenticator: no PublicKeyCredential is ever made, so its members are only
/// reachable through the prototype (where the generated code rejects a wrong `this`).
fn no_public_key_credential() -> Error {
    Error::Type("Illegal invocation".into())
}

impl PublicKeyCredentialImpl for Engine {
    fn raw_id(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn response(&self, this: Handle) -> Handle {
        this
    }
    fn authenticator_attachment(&self, _this: Handle) -> Option<DomStr<'_>> {
        None
    }
    fn get_client_extension_results(
        &self,
        _cx: &mut Cx,
        _this: Handle,
    ) -> rt::Result<AuthenticationExtensionsClientOutputs> {
        Err(no_public_key_credential())
    }
    fn to_json(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn static_is_conditional_mediation_available(&self, cx: &mut Cx) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        p.resolve(cx, false);
        Ok(p)
    }
    /// No platform authenticator.
    fn static_is_user_verifying_platform_authenticator_available(
        &self,
        cx: &mut Cx,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        p.resolve(cx, false);
        Ok(p)
    }
    /// WebAuthn §5.1.? `getClientCapabilities()`: an empty record (no capability is claimed).
    fn static_get_client_capabilities(&self, cx: &mut Cx) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        let record = {
            let scope = cx.scope();
            let o: v8::Local<v8::Value> = v8::Object::new(scope).into();
            JsValue::new(scope, o)
        };
        p.resolve(cx, record);
        Ok(p)
    }
    fn static_parse_creation_options_from_json(
        &self,
        _cx: &mut Cx,
        _options: PublicKeyCredentialCreationOptionsJSON,
    ) -> rt::Result<PublicKeyCredentialCreationOptions> {
        Err(exception(
            ExceptionName::NotSupportedError,
            "Public key credentials are not supported",
        ))
    }
    fn static_parse_request_options_from_json(
        &self,
        _cx: &mut Cx,
        _options: PublicKeyCredentialRequestOptionsJSON,
    ) -> rt::Result<PublicKeyCredentialRequestOptions> {
        Err(exception(
            ExceptionName::NotSupportedError,
            "Public key credentials are not supported",
        ))
    }
    /// The signal methods tell authenticators about credentials; with none, there is no one to tell.
    fn static_signal_unknown_credential(
        &self,
        cx: &mut Cx,
        _options: UnknownCredentialOptions,
    ) -> rt::Result<JsPromise> {
        super::element::resolved(cx)
    }
    fn static_signal_all_accepted_credentials(
        &self,
        cx: &mut Cx,
        _options: AllAcceptedCredentialsOptions,
    ) -> rt::Result<JsPromise> {
        super::element::resolved(cx)
    }
    fn static_signal_current_user_details(
        &self,
        cx: &mut Cx,
        _options: CurrentUserDetailsOptions,
    ) -> rt::Result<JsPromise> {
        super::element::resolved(cx)
    }
}

impl AuthenticatorResponseImpl for Engine {
    fn client_data_json(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
}

impl AuthenticatorAttestationResponseImpl for Engine {
    fn attestation_object(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn get_transports(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Vec<String>> {
        Err(no_public_key_credential())
    }
    fn get_authenticator_data(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn get_public_key(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Option<JsObject>> {
        Err(no_public_key_credential())
    }
    fn get_public_key_algorithm(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<i32> {
        Err(no_public_key_credential())
    }
}

impl AuthenticatorAssertionResponseImpl for Engine {
    fn authenticator_data(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn signature(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsObject> {
        Err(no_public_key_credential())
    }
    fn user_handle(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Option<JsObject>> {
        Err(no_public_key_credential())
    }
}

/// FedCM without an identity provider: no IdentityCredential is ever made.
impl IdentityCredentialImpl for Engine {
    fn token(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<JsValue> {
        Err(no_public_key_credential())
    }
    fn is_auto_selected(&self, _this: Handle) -> bool {
        false
    }
    fn config_url(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    /// FedCM "disconnect": the identity provider's config cannot be fetched here.
    fn static_disconnect(
        &self,
        cx: &mut Cx,
        _options: IdentityCredentialDisconnectOptions,
    ) -> rt::Result<JsPromise> {
        self.rejected_promise(
            cx,
            exception(
                ExceptionName::NetworkError,
                "No identity provider is available",
            ),
        )
    }
}
