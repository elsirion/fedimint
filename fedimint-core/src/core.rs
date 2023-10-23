//! Fedimint Core API (common) module interface
//!
//! Fedimint supports externally implemented modules.
//!
//! This (Rust) module defines common interoperability types
//! and functionality that is used on both client and sever side.
use core::fmt;
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::anyhow;
pub use bitcoin::KeyPair;
use fedimint_core::encoding::{Decodable, DecodeError, DynEncodable, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use rand::RngCore;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    erased_eq_no_instance_id, module_plugin_dyn_newtype_clone_passthrough,
    module_plugin_dyn_newtype_define, module_plugin_dyn_newtype_display_passthrough,
    module_plugin_dyn_newtype_encode_decode, module_plugin_dyn_newtype_eq_passthrough,
    module_plugin_static_trait_define, module_plugin_static_trait_define_config,
};

pub mod client;
pub mod server;

pub mod backup;

/// Unique identifier for one semantic, correlatable operation.
///
/// The concept of *operations* is used to avoid losing privacy while being as
/// efficient as possible with regards to network requests.
///
/// For Fedimint transactions to be private users need to communicate with the
/// federation using an anonymous communication network. If each API request was
/// done in a way that it cannot be correlated to any other API request we would
/// achieve privacy, but would reduce efficiency. E.g. on Tor we would need to
/// open a new circuit for every request and open a new web socket connection.
///
/// Fortunately we do not need to do that to maintain privacy. Many API requests
/// and transactions can be correlated by the federation anyway, in these cases
/// it does not make any difference to re-use the same network connection. All
/// requests, transactions, state machines that are connected from the
/// federation's point of view anyway are grouped together as one *operation*.
///
/// # Choice of Operation ID
///
/// In cases where an operation is created by a new transaction that's being
/// submitted the transaction's ID can be used as operation ID. If there is no
/// transaction related to it, it should be generated randomly. Since it is a
/// 256bit value collisions are impossible for all intents and purposes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct OperationId(pub [u8; 32]);

impl OperationId {
    /// Generate random [`OperationId`]
    pub fn new_random() -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}

impl Display for OperationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        bitcoin_hashes::hex::format_hex(&self.0, f)
    }
}

impl Debug for OperationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "OperationId({self})")
    }
}

impl FromStr for OperationId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 32] = bitcoin_hashes::hex::FromHex::from_hex(s)?;
        Ok(OperationId(bytes))
    }
}

impl Serialize for OperationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_string())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let operation_id = OperationId::from_str(&s)
                .map_err(|e| serde::de::Error::custom(format!("invalid operation id: {e}")))?;
            Ok(operation_id)
        } else {
            let bytes: [u8; 32] = <[u8; 32]>::deserialize(deserializer)?;
            Ok(OperationId(bytes))
        }
    }
}

/// Module instance ID
///
/// This value uniquely identifies a single instance of a module in a
/// federation.
///
/// In case a single [`ModuleKind`] is instantiated twice (rare, but possible),
/// each instance will have a different id.
///
/// Note: We have used this type differently before, assuming each `u16`
/// uniquly identifies a type of module in question. This function will move
/// to a `ModuleKind` type which only identifies type of a module (mint vs
/// wallet vs ln, etc)
// TODO: turn in a newtype
pub type ModuleInstanceId = u16;

/// Special IDs we use for global dkg
pub const MODULE_INSTANCE_ID_GLOBAL: u16 = u16::MAX;

// Note: needs to be in alphabetical order of ModuleKind of each module,
// as this is the ordering we currently hardcoded.
// Should be used only for pre-modularization code we still have  left
pub const LEGACY_HARDCODED_INSTANCE_ID_LN: ModuleInstanceId = 0;
pub const LEGACY_HARDCODED_INSTANCE_ID_MINT: ModuleInstanceId = 1;
pub const LEGACY_HARDCODED_INSTANCE_ID_WALLET: ModuleInstanceId = 2;

/// A type of a module
///
/// This is a short string that identifies type of a module.
/// Authors of 3rd party modules are free to come up with a string,
/// long enough to avoid conflicts with similar modules.
#[derive(
    Debug, PartialEq, Eq, Clone, PartialOrd, Ord, Serialize, Deserialize, Encodable, Decodable,
)]
pub struct ModuleKind(Cow<'static, str>);

impl ModuleKind {
    pub fn clone_from_str(s: &str) -> Self {
        Self(Cow::from(s.to_owned()))
    }

    pub const fn from_static_str(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<&'static str> for ModuleKind {
    fn from(val: &'static str) -> Self {
        ModuleKind::from_static_str(val)
    }
}

/// A type used by when decoding dyn-types, when the module is missing
///
/// This allows parsing and handling of dyn-types of modules which
/// are not available.
#[derive(Encodable, Decodable, Debug, Hash, PartialEq, Clone)]
pub struct DynUnknown(Vec<u8>);

impl fmt::Display for DynUnknown {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.consensus_encode_to_hex().expect("can't fail"))
    }
}

/// A type that has a `Dyn*`, type erased version of itself
pub trait IntoDynInstance {
    /// The type erased version of the type implementing this trait
    type DynType: 'static;

    /// Convert `self` into its type-erased equivalent
    fn into_dyn(self, instance_id: ModuleInstanceId) -> Self::DynType;
}

type DecodeFn = for<'a> fn(
    Box<dyn Read + 'a>,
    ModuleInstanceId,
    &ModuleDecoderRegistry,
) -> Result<Box<dyn Any>, DecodeError>;

#[derive(Default)]
pub struct DecoderBuilder {
    decode_fns: BTreeMap<TypeId, DecodeFn>,
}

impl DecoderBuilder {
    pub fn build(self) -> Decoder {
        Decoder {
            decode_fns: Arc::new(self.decode_fns),
        }
    }

    /// Attach decoder for a specific `Type`/`DynType` pair where `DynType =
    /// <Type as IntoDynInstance>::DynType`.
    ///
    /// This allows calling `decode::<DynType>` on this decoder, returning a
    /// `DynType` object which contains a `Type` object internally.
    ///
    /// **Caution**: One `Decoder` object should only contain decoders that
    /// belong to the same [*module kind*](fedimint_core::core::ModuleKind).
    ///
    /// # Panics
    /// * If multiple `Types` with the same `DynType` are added
    pub fn with_decodable_type<Type>(&mut self)
    where
        Type: IntoDynInstance + Decodable,
    {
        // TODO: enforce that all decoders are for the same module kind (+fix docs
        // after)
        let decode_fn: DecodeFn = |mut reader, instance, modules| {
            let typed_val = Type::consensus_decode(&mut reader, modules)?;
            let dyn_val = typed_val.into_dyn(instance);
            let any_val: Box<dyn Any> = Box::new(dyn_val);
            Ok(any_val)
        };
        if self
            .decode_fns
            .insert(TypeId::of::<Type::DynType>(), decode_fn)
            .is_some()
        {
            panic!("Tried to add multiple decoders for the same DynType");
        }
    }
}

/// Decoder for module associated types
#[derive(Clone, Default)]
pub struct Decoder {
    decode_fns: Arc<BTreeMap<TypeId, DecodeFn>>,
}

impl Decoder {
    /// Creates a `DecoderBuilder` to which decoders for single types can be
    /// attached to build a `Decoder`.
    pub fn builder() -> DecoderBuilder {
        DecoderBuilder::default()
    }

    /// Decodes a specific `DynType` from the `reader` byte stream.
    ///
    /// # Panics
    /// * If no decoder is registered for the `DynType`
    pub fn decode<DynType: Any>(
        &self,
        reader: &mut dyn Read,
        instance_id: ModuleInstanceId,
        modules: &ModuleDecoderRegistry,
    ) -> Result<DynType, DecodeError> {
        let decode_fn = self
            .decode_fns
            .get(&TypeId::of::<DynType>())
            .ok_or_else(|| {
                anyhow!(
                    "Type unknown to decoder: {}, (registered decoders={})",
                    std::any::type_name::<DynType>(),
                    self.decode_fns.len()
                )
            })
            .expect("Types being decoded must be registered");
        Ok(*decode_fn(Box::new(reader), instance_id, modules)?
            .downcast::<DynType>()
            .expect("Decode fn returned wrong type, can't happen due to with_decodable_type"))
    }
}

impl Debug for Decoder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decoder(registered_types = {})", self.decode_fns.len())
    }
}

pub trait IClientConfig: Debug + Display + DynEncodable {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn clone(&self, instance_id: ModuleInstanceId) -> DynClientConfig;
    fn dyn_hash(&self) -> u64;
    fn erased_eq_no_instance_id(&self, other: &DynClientConfig) -> bool;
    fn to_json(&self) -> Option<serde_json::Value>;
}

module_plugin_static_trait_define_config! {
    DynClientConfig, ClientConfig, IClientConfig,
    { },
    {
        erased_eq_no_instance_id!(DynClientConfig);

        fn to_json(&self) -> Option<serde_json::Value> {
            Some(serde_json::to_value(self.to_owned()).expect("serialization can't fail"))
        }
    },
    {
        erased_eq_no_instance_id!(DynClientConfig);

        fn to_json(&self) -> Option<serde_json::Value> {
            None
        }
    }
}

module_plugin_dyn_newtype_define! {
    /// An owned, immutable input to a [`Transaction`](fedimint_core::transaction::Transaction)
    pub DynClientConfig(Box<IClientConfig>)
}
module_plugin_dyn_newtype_encode_decode!(DynClientConfig);

module_plugin_dyn_newtype_clone_passthrough!(DynClientConfig);

module_plugin_dyn_newtype_eq_passthrough!(DynClientConfig);

module_plugin_dyn_newtype_display_passthrough!(DynClientConfig);

/// Something that can be an [`DynInput`] in a
/// [`Transaction`](fedimint_core::transaction::Transaction)
///
/// General purpose code should use [`DynInput`] instead
pub trait IInput: Debug + Display + DynEncodable {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn clone(&self, instance_id: ModuleInstanceId) -> DynInput;
    fn dyn_hash(&self) -> u64;
    fn erased_eq_no_instance_id(&self, other: &DynInput) -> bool;
}

module_plugin_static_trait_define! {
    DynInput, Input, IInput,
    { },
    {
        erased_eq_no_instance_id!(DynInput);
    }
}

module_plugin_dyn_newtype_define! {
    /// An owned, immutable input to a [`Transaction`](fedimint_core::transaction::Transaction)
    pub DynInput(Box<IInput>)
}
module_plugin_dyn_newtype_encode_decode!(DynInput);

module_plugin_dyn_newtype_clone_passthrough!(DynInput);

module_plugin_dyn_newtype_eq_passthrough!(DynInput);

module_plugin_dyn_newtype_display_passthrough!(DynInput);

/// Something that can be an [`DynOutput`] in a
/// [`Transaction`](fedimint_core::transaction::Transaction)
///
/// General purpose code should use [`DynOutput`] instead
pub trait IOutput: Debug + Display + DynEncodable {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn clone(&self, instance_id: ModuleInstanceId) -> DynOutput;
    fn dyn_hash(&self) -> u64;
    fn erased_eq_no_instance_id(&self, other: &DynOutput) -> bool;
}

module_plugin_dyn_newtype_define! {
    /// An owned, immutable output of a [`Transaction`](fedimint_core::transaction::Transaction)
    pub DynOutput(Box<IOutput>)
}
module_plugin_static_trait_define! {
    DynOutput, Output, IOutput,
    { },
    {
        erased_eq_no_instance_id!(DynOutput);
    }
}
module_plugin_dyn_newtype_encode_decode!(DynOutput);

module_plugin_dyn_newtype_clone_passthrough!(DynOutput);

module_plugin_dyn_newtype_eq_passthrough!(DynOutput);

module_plugin_dyn_newtype_display_passthrough!(DynOutput);

pub enum FinalizationError {
    SomethingWentWrong,
}

pub trait IOutputOutcome: Debug + Display + DynEncodable {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn clone(&self, module_instance_id: ModuleInstanceId) -> DynOutputOutcome;
    fn dyn_hash(&self) -> u64;
    fn erased_eq_no_instance_id(&self, other: &DynOutputOutcome) -> bool;
}

module_plugin_dyn_newtype_define! {
    /// An owned, immutable output of a [`Transaction`](fedimint_core::transaction::Transaction) before it was finalized
    pub DynOutputOutcome(Box<IOutputOutcome>)
}
module_plugin_static_trait_define! {
    DynOutputOutcome, OutputOutcome, IOutputOutcome,
    { },
    {
        erased_eq_no_instance_id!(DynOutputOutcome);
    }
}
module_plugin_dyn_newtype_encode_decode!(DynOutputOutcome);
module_plugin_dyn_newtype_clone_passthrough!(DynOutputOutcome);
module_plugin_dyn_newtype_eq_passthrough!(DynOutputOutcome);
module_plugin_dyn_newtype_display_passthrough!(DynOutputOutcome);

pub trait IModuleConsensusItem: Debug + Display + DynEncodable {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn clone(&self, module_instance_id: ModuleInstanceId) -> DynModuleConsensusItem;
    fn dyn_hash(&self) -> u64;

    fn erased_eq_no_instance_id(&self, other: &DynModuleConsensusItem) -> bool;
}

module_plugin_dyn_newtype_define! {
    /// An owned, immutable output of a [`Transaction`](fedimint_core::transaction::Transaction) before it was finalized
    pub DynModuleConsensusItem(Box<IModuleConsensusItem>)
}
module_plugin_static_trait_define! {
    DynModuleConsensusItem, ModuleConsensusItem, IModuleConsensusItem,
    { },
    {
        erased_eq_no_instance_id!(DynModuleConsensusItem);
    }
}
module_plugin_dyn_newtype_encode_decode!(DynModuleConsensusItem);

module_plugin_dyn_newtype_clone_passthrough!(DynModuleConsensusItem);

module_plugin_dyn_newtype_eq_passthrough!(DynModuleConsensusItem);

module_plugin_dyn_newtype_display_passthrough!(DynModuleConsensusItem);
