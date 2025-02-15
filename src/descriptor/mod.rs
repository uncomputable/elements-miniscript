// Written in 2018 by Andrew Poelstra <apoelstra@wpsoftware.net>
// SPDX-License-Identifier: CC0-1.0

//! # Output Descriptors
//!
//! Tools for representing Bitcoin output's scriptPubKeys as abstract spending
//! policies known as "output descriptors". These include a Miniscript which
//! describes the actual signing policy, as well as the blockchain format (P2SH,
//! Segwit v0, etc.)
//!
//! The format represents EC public keys abstractly to allow wallets to replace
//! these with BIP32 paths, pay-to-contract instructions, etc.
//!

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::str::{self, FromStr};
use std::sync::Arc;

pub mod pegin;

use bitcoin::address::WitnessVersion;
use elements::hashes::{hash160, ripemd160, sha256};
use elements::{secp256k1_zkp as secp256k1, secp256k1_zkp, Script, TxIn};
use {bitcoin, elements};

use self::checksum::verify_checksum;
use crate::extensions::{CovExtArgs, ExtParam, ParseableExt};
use crate::miniscript::{Legacy, Miniscript, Segwitv0};
use crate::{
    expression, hash256, miniscript, BareCtx, CovenantExt, Error, ExtTranslator, Extension,
    ForEachKey, MiniscriptKey, NoExt, Satisfier, ToPublicKey, TranslateExt, TranslatePk,
    Translator,
};

mod bare;
mod csfs_cov;
mod segwitv0;
mod sh;
mod sortedmulti;
mod tr;

// Descriptor Exports
pub use self::bare::{Bare, Pkh};
pub use self::segwitv0::{Wpkh, Wsh, WshInner};
pub use self::sh::{Sh, ShInner};
pub use self::sortedmulti::SortedMultiVec;

pub mod checksum;
mod key;
pub use self::csfs_cov::{CovError, CovOperations, LegacyCSFSCov, LegacyCovSatisfier};
pub use self::key::{
    ConversionError, DefiniteDescriptorKey, DerivPaths, DescriptorKeyParseError,
    DescriptorMultiXKey, DescriptorPublicKey, DescriptorSecretKey, DescriptorXKey, InnerXKey,
    SinglePriv, SinglePub, SinglePubKey, Wildcard,
};
pub use self::tr::{TapTree, Tr, TapLeafScript};
/// Alias type for a map of public key to secret key
///
/// This map is returned whenever a descriptor that contains secrets is parsed using
/// [`Descriptor::parse_descriptor`], since the descriptor will always only contain
/// public keys. This map allows looking up the corresponding secret key given a
/// public key from the descriptor.
pub type KeyMap = HashMap<DescriptorPublicKey, DescriptorSecretKey>;

/// Elements Descriptor String Prefix
pub const ELMTS_STR: &str = "el";

/// Descriptor Type of the descriptor
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DescriptorType {
    /// Bare descriptor(Contains the native P2pk)
    Bare,
    /// Pure Sh Descriptor. Does not contain nested Wsh/Wpkh
    Sh,
    /// Pkh Descriptor
    Pkh,
    /// Wpkh Descriptor
    Wpkh,
    /// Wsh
    Wsh,
    /// Sh Wrapped Wsh
    ShWsh,
    /// Sh wrapped Wpkh
    ShWpkh,
    /// Sh Sorted Multi
    ShSortedMulti,
    /// Wsh Sorted Multi
    WshSortedMulti,
    /// Sh Wsh Sorted Multi
    ShWshSortedMulti,
    /// Legacy Pegin
    LegacyPegin,
    /// Dynafed Pegin
    Pegin,
    /// Covenant: Only supported in p2wsh context
    Cov,
    /// Tr
    Tr,
}

impl fmt::Display for DescriptorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DescriptorType::Bare => write!(f, "bare"),
            DescriptorType::Sh => write!(f, "sh"),
            DescriptorType::Pkh => write!(f, "pkh"),
            DescriptorType::Wpkh => write!(f, "wpkh"),
            DescriptorType::Wsh => write!(f, "wsh"),
            DescriptorType::ShWsh => write!(f, "shwsh"),
            DescriptorType::ShWpkh => write!(f, "shwpkh"),
            DescriptorType::ShSortedMulti => write!(f, "shsortedmulti"),
            DescriptorType::WshSortedMulti => write!(f, "wshsortedmulti"),
            DescriptorType::ShWshSortedMulti => write!(f, "shwshsortedmulti"),
            DescriptorType::LegacyPegin => write!(f, "legacy_pegin"),
            DescriptorType::Pegin => write!(f, "pegin"),
            DescriptorType::Cov => write!(f, "elcovwsh"),
            DescriptorType::Tr => write!(f, "tr"),
        }
    }
}
impl FromStr for DescriptorType {
    type Err = Error;

    /// Does not check if the Descriptor is well formed or not.
    /// Such errors would be caught later while parsing the descriptor
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() >= 12 && &s[0..12] == "legacy_pegin" {
            Ok(DescriptorType::LegacyPegin)
        } else if s.len() >= 5 && &s[0..5] == "pegin" {
            Ok(DescriptorType::Pegin)
        } else if s.len() >= 3 && &s[0..3] == "pkh" {
            Ok(DescriptorType::Pkh)
        } else if s.len() >= 4 && &s[0..4] == "wpkh" {
            Ok(DescriptorType::Wpkh)
        } else if s.len() >= 6 && &s[0..6] == "sh(wsh" {
            Ok(DescriptorType::ShWsh)
        } else if s.len() >= 7 && &s[0..7] == "sh(wpkh" {
            Ok(DescriptorType::ShWpkh)
        } else if s.len() >= 14 && &s[0..14] == "sh(sortedmulti" {
            Ok(DescriptorType::ShSortedMulti)
        } else if s.len() >= 18 && &s[0..18] == "sh(wsh(sortedmulti" {
            Ok(DescriptorType::ShWshSortedMulti)
        } else if s.len() >= 2 && &s[0..2] == "sh" {
            Ok(DescriptorType::Sh)
        } else if s.len() >= 15 && &s[0..15] == "wsh(sortedmulti" {
            Ok(DescriptorType::WshSortedMulti)
        } else if s.len() >= 3 && &s[0..3] == "wsh" {
            Ok(DescriptorType::Wsh)
        } else if s.len() >= 6 && &s[0..6] == "covwsh" {
            Ok(DescriptorType::Cov)
        } else {
            Ok(DescriptorType::Bare)
        }
    }
}
/// Method for determining Type of descriptor when parsing from String
pub enum DescriptorInfo {
    /// Bitcoin Descriptor
    Btc {
        /// Whether descriptor has secret keys
        has_secret: bool,
        /// The type of descriptor
        ty: DescriptorType,
    },
    /// Elements Descriptor
    Elements {
        /// Whether descriptor has secret keys
        has_secret: bool,
        /// The type of descriptor
        ty: DescriptorType,
    },
    /// Pegin descriptor
    /// Only provides information about the bitcoin side of descriptor
    /// Use the corresponding [`pegin::LegacyPegin::into_user_descriptor`] or
    /// [`pegin::Pegin::into_user_descriptor`] method to obtain the user descriptor.
    /// and call DescriptorType method on it on to find information about
    /// the user claim descriptor.
    Pegin {
        /// Whether the user descriptor has secret
        has_secret: bool,
        /// The type of descriptor
        ty: DescriptorType,
    },
}

impl DescriptorInfo {
    /// Compute the [`DescriptorInfo`] for the given descriptor string
    /// This method should when the user is unsure whether they are parsing
    /// Bitcoin Descriptor, Elements Descriptor or Pegin Descriptors.
    /// This also returns information whether the descriptor contains any secrets
    /// of the type [`DescriptorSecretKey`]. If the descriptor contains secret, users
    /// should use the method [`Descriptor::parse_descriptor`] to obtain the
    /// Descriptor and a secret key to public key mapping
    pub fn from_desc_str<T: Extension>(s: &str) -> Result<Self, Error> {
        // Parse as a string descriptor
        let descriptor = Descriptor::<String, T>::from_str(s)?;
        let has_secret = descriptor.for_any_key(|pk| DescriptorSecretKey::from_str(pk).is_ok());
        let ty = DescriptorType::from_str(s)?;
        let is_pegin = matches!(ty, DescriptorType::Pegin | DescriptorType::LegacyPegin);
        // Todo: add elements later
        if is_pegin {
            Ok(DescriptorInfo::Pegin { has_secret, ty })
        } else {
            Ok(DescriptorInfo::Btc { has_secret, ty })
        }
    }
}

/// Script descriptor
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Descriptor<Pk: MiniscriptKey, T: Extension = CovenantExt<CovExtArgs>> {
    /// A raw scriptpubkey (including pay-to-pubkey) under Legacy context
    Bare(Bare<Pk>),
    /// Pay-to-PubKey-Hash
    Pkh(Pkh<Pk>),
    /// Pay-to-Witness-PubKey-Hash
    Wpkh(Wpkh<Pk>),
    /// Pay-to-ScriptHash(includes nested wsh/wpkh/sorted multi)
    Sh(Sh<Pk>),
    /// Pay-to-Witness-ScriptHash with Segwitv0 context
    Wsh(Wsh<Pk>),
    /// Pay-to-Taproot
    Tr(Tr<Pk, NoExt>),
    /// Pay-to-Taproot
    TrExt(Tr<Pk, T>),
    /// Covenant descriptor with all known extensions
    /// Downstream implementations of extensions should implement directly use descriptor API
    LegacyCSFSCov(LegacyCSFSCov<Pk, T>),
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Bare<Pk>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Bare<Pk>) -> Self {
        Descriptor::Bare(inner)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Pkh<Pk>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Pkh<Pk>) -> Self {
        Descriptor::Pkh(inner)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Wpkh<Pk>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Wpkh<Pk>) -> Self {
        Descriptor::Wpkh(inner)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Sh<Pk>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Sh<Pk>) -> Self {
        Descriptor::Sh(inner)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Wsh<Pk>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Wsh<Pk>) -> Self {
        Descriptor::Wsh(inner)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> From<Tr<Pk, NoExt>> for Descriptor<Pk, Ext> {
    #[inline]
    fn from(inner: Tr<Pk, NoExt>) -> Self {
        Descriptor::Tr(inner)
    }
}

impl<Pk: MiniscriptKey, Arg: ExtParam> From<LegacyCSFSCov<Pk, CovenantExt<Arg>>>
    for Descriptor<Pk, CovenantExt<Arg>>
{
    #[inline]
    fn from(inner: LegacyCSFSCov<Pk, CovenantExt<Arg>>) -> Self {
        Descriptor::LegacyCSFSCov(inner)
    }
}

impl DescriptorType {
    /// Returns the segwit version implied by the descriptor type.
    ///
    /// This will return `Some(WitnessVersion::V0)` whether it is "native" segwitv0 or "wrapped" p2sh segwit.
    pub fn segwit_version(&self) -> Option<WitnessVersion> {
        use self::DescriptorType::*;
        match self {
            Tr => Some(WitnessVersion::V1),
            Wpkh | ShWpkh | Wsh | ShWsh | ShWshSortedMulti | WshSortedMulti => {
                Some(WitnessVersion::V0)
            }
            Bare | Sh | Pkh | ShSortedMulti => None,
            LegacyPegin => Some(WitnessVersion::V1),
            Pegin => None, // Can have any witness version
            Cov => None,   // Can have any witness version
        }
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> Descriptor<Pk, Ext> {
    // Keys

    /// Create a new pk descriptor
    pub fn new_pk(pk: Pk) -> Self {
        // roundabout way to constuct `c:pk_k(pk)`
        let ms: Miniscript<Pk, BareCtx> =
            Miniscript::from_ast(miniscript::decode::Terminal::Check(Arc::new(
                Miniscript::from_ast(miniscript::decode::Terminal::PkK(pk))
                    .expect("Type check cannot fail"),
            )))
            .expect("Type check cannot fail");
        Descriptor::Bare(Bare::new(ms).expect("Context checks cannot fail for p2pk"))
    }

    /// Create a new PkH descriptor
    pub fn new_pkh(pk: Pk) -> Self {
        Descriptor::Pkh(Pkh::new(pk))
    }

    /// Create a new Wpkh descriptor
    /// Will return Err if uncompressed key is used
    pub fn new_wpkh(pk: Pk) -> Result<Self, Error> {
        Ok(Descriptor::Wpkh(Wpkh::new(pk)?))
    }

    /// Create a new sh wrapped wpkh from `Pk`.
    /// Errors when uncompressed keys are supplied
    pub fn new_sh_wpkh(pk: Pk) -> Result<Self, Error> {
        Ok(Descriptor::Sh(Sh::new_wpkh(pk)?))
    }

    // Miniscripts

    /// Create a new sh for a given redeem script
    /// Errors when miniscript exceeds resource limits under p2sh context
    /// or does not type check at the top level
    pub fn new_sh(ms: Miniscript<Pk, Legacy>) -> Result<Self, Error> {
        Ok(Descriptor::Sh(Sh::new(ms)?))
    }

    /// Create a new wsh descriptor from witness script
    /// Errors when miniscript exceeds resource limits under p2sh context
    /// or does not type check at the top level
    pub fn new_wsh(ms: Miniscript<Pk, Segwitv0>) -> Result<Self, Error> {
        Ok(Descriptor::Wsh(Wsh::new(ms)?))
    }

    /// Create a new sh wrapped wsh descriptor with witness script
    /// Errors when miniscript exceeds resource limits under wsh context
    /// or does not type check at the top level
    pub fn new_sh_wsh(ms: Miniscript<Pk, Segwitv0>) -> Result<Self, Error> {
        Ok(Descriptor::Sh(Sh::new_wsh(ms)?))
    }

    /// Create a new bare descriptor from witness script
    /// Errors when miniscript exceeds resource limits under bare context
    /// or does not type check at the top level
    pub fn new_bare(ms: Miniscript<Pk, BareCtx>) -> Result<Self, Error> {
        Ok(Descriptor::Bare(Bare::new(ms)?))
    }

    // Wrap with sh

    /// Create a new sh wrapper for the given wpkh descriptor
    pub fn new_sh_with_wpkh(wpkh: Wpkh<Pk>) -> Self {
        Descriptor::Sh(Sh::new_with_wpkh(wpkh))
    }

    /// Create a new sh wrapper for the given wsh descriptor
    pub fn new_sh_with_wsh(wsh: Wsh<Pk>) -> Self {
        Descriptor::Sh(Sh::new_with_wsh(wsh))
    }

    // sorted multi

    /// Create a new sh sortedmulti descriptor with threshold `k`
    /// and Vec of `pks`.
    /// Errors when miniscript exceeds resource limits under p2sh context
    pub fn new_sh_sortedmulti(k: usize, pks: Vec<Pk>) -> Result<Self, Error> {
        Ok(Descriptor::Sh(Sh::new_sortedmulti(k, pks)?))
    }

    /// Create a new sh wrapped wsh sortedmulti descriptor from threshold
    /// `k` and Vec of `pks`
    /// Errors when miniscript exceeds resource limits under segwit context
    pub fn new_sh_wsh_sortedmulti(k: usize, pks: Vec<Pk>) -> Result<Self, Error> {
        Ok(Descriptor::Sh(Sh::new_wsh_sortedmulti(k, pks)?))
    }

    /// Create a new wsh sorted multi descriptor
    /// Errors when miniscript exceeds resource limits under p2sh context
    pub fn new_wsh_sortedmulti(k: usize, pks: Vec<Pk>) -> Result<Self, Error> {
        Ok(Descriptor::Wsh(Wsh::new_sortedmulti(k, pks)?))
    }

    /// Create new tr descriptor
    /// Errors when miniscript exceeds resource limits under Tap context
    pub fn new_tr(key: Pk, script: Option<tr::TapTree<Pk, NoExt>>) -> Result<Self, Error> {
        Ok(Descriptor::Tr(Tr::new(key, script)?))
    }

    /// Create new tr descriptor
    /// Errors when miniscript exceeds resource limits under Tap context
    pub fn new_tr_ext(key: Pk, script: Option<tr::TapTree<Pk, Ext>>) -> Result<Self, Error> {
        Ok(Descriptor::TrExt(Tr::new(key, script)?))
    }

    /// Get the [DescriptorType] of [Descriptor]
    pub fn desc_type(&self) -> DescriptorType {
        match *self {
            Descriptor::Bare(ref _bare) => DescriptorType::Bare,
            Descriptor::Pkh(ref _pkh) => DescriptorType::Pkh,
            Descriptor::Wpkh(ref _wpkh) => DescriptorType::Wpkh,
            Descriptor::Sh(ref sh) => match sh.as_inner() {
                ShInner::Wsh(ref wsh) => match wsh.as_inner() {
                    WshInner::SortedMulti(ref _smv) => DescriptorType::ShWshSortedMulti,
                    WshInner::Ms(ref _ms) => DescriptorType::ShWsh,
                },
                ShInner::Wpkh(ref _wpkh) => DescriptorType::ShWpkh,
                ShInner::SortedMulti(ref _smv) => DescriptorType::ShSortedMulti,
                ShInner::Ms(ref _ms) => DescriptorType::Sh,
            },
            Descriptor::Wsh(ref wsh) => match wsh.as_inner() {
                WshInner::SortedMulti(ref _smv) => DescriptorType::WshSortedMulti,
                WshInner::Ms(ref _ms) => DescriptorType::Wsh,
            },
            Descriptor::LegacyCSFSCov(ref _cov) => DescriptorType::Cov,
            Descriptor::Tr(ref _tr) => DescriptorType::Tr,
            Descriptor::TrExt(ref _tr) => DescriptorType::Tr,
        }
    }

    /// Return a string without the checksum
    pub fn to_string_no_chksum(&self) -> String {
        format!("{:?}", self)
    }
    /// Checks whether the descriptor is safe.
    ///
    /// Checks whether all the spend paths in the descriptor are possible on the
    /// bitcoin network under the current standardness and consensus rules. Also
    /// checks whether the descriptor requires signatures on all spend paths and
    /// whether the script is malleable.
    ///
    /// In general, all the guarantees of miniscript hold only for safe scripts.
    /// The signer may not be able to find satisfactions even if one exists.
    pub fn sanity_check(&self) -> Result<(), Error> {
        match *self {
            Descriptor::Bare(ref bare) => bare.sanity_check(),
            Descriptor::Pkh(_) => Ok(()),
            Descriptor::Wpkh(ref wpkh) => wpkh.sanity_check(),
            Descriptor::Wsh(ref wsh) => wsh.sanity_check(),
            Descriptor::Sh(ref sh) => sh.sanity_check(),
            Descriptor::LegacyCSFSCov(ref cov) => cov.sanity_check(),
            Descriptor::Tr(ref tr) => tr.sanity_check(),
            Descriptor::TrExt(ref tr) => tr.sanity_check(),
        }
    }

    /// Computes an upper bound on the difference between a non-satisfied
    /// `TxIn`'s `segwit_weight` and a satisfied `TxIn`'s `segwit_weight`
    ///
    /// Since this method uses `segwit_weight` instead of `legacy_weight`,
    /// if you want to include only legacy inputs in your transaction,
    /// you should remove 1WU from each input's `max_weight_to_satisfy`
    /// for a more accurate estimate.
    ///
    /// In other words, for segwit inputs or legacy inputs included in
    /// segwit transactions, the following will hold for each input if
    /// that input was satisfied with the largest possible witness:
    /// ```ignore
    /// for i in 0..transaction.input.len() {
    ///     assert_eq!(
    ///         descriptor_for_input[i].max_weight_to_satisfy(),
    ///         transaction.input[i].segwit_weight() - Txin::default().segwit_weight()
    ///     );
    /// }
    /// ```
    ///
    /// Instead, for legacy transactions, the following will hold for each input
    /// if that input was satisfied with the largest possible witness:
    /// ```ignore
    /// for i in 0..transaction.input.len() {
    ///     assert_eq!(
    ///         descriptor_for_input[i].max_weight_to_satisfy(),
    ///         transaction.input[i].legacy_weight() - Txin::default().legacy_weight()
    ///     );
    /// }
    /// ```
    ///
    /// Assumes all ECDSA signatures are 73 bytes, including push opcode and
    /// sighash suffix.
    /// Assumes all Schnorr signatures are 66 bytes, including push opcode and
    /// sighash suffix.
    ///
    /// # Errors
    /// When the descriptor is impossible to safisfy (ex: sh(OP_FALSE)).
    pub fn max_weight_to_satisfy(&self) -> Result<usize, Error> {
        let weight = match *self {
            Descriptor::Bare(ref bare) => bare.max_weight_to_satisfy()?,
            Descriptor::Pkh(ref pkh) => pkh.max_weight_to_satisfy(),
            Descriptor::Wpkh(ref wpkh) => wpkh.max_weight_to_satisfy(),
            Descriptor::Wsh(ref wsh) => wsh.max_weight_to_satisfy()?,
            Descriptor::Sh(ref sh) => sh.max_weight_to_satisfy()?,
            Descriptor::Tr(ref tr) => tr.max_weight_to_satisfy()?,
            Descriptor::TrExt(ref tr) => tr.max_weight_to_satisfy()?,
            Descriptor::LegacyCSFSCov(ref csfs) => csfs.max_satisfaction_weight()?,
        };
        Ok(weight)
    }

    /// Computes an upper bound on the weight of a satisfying witness to the
    /// transaction.
    ///
    /// Assumes all ec-signatures are 73 bytes, including push opcode and
    /// sighash suffix. Includes the weight of the VarInts encoding the
    /// scriptSig and witness stack length.
    ///
    /// # Errors
    /// When the descriptor is impossible to safisfy (ex: sh(OP_FALSE)).
    #[deprecated(note = "use max_weight_to_satisfy instead")]
    #[allow(deprecated)]
    pub fn max_satisfaction_weight(&self) -> Result<usize, Error> {
        let weight = match *self {
            Descriptor::Bare(ref bare) => bare.max_satisfaction_weight()?,
            Descriptor::Pkh(ref pkh) => pkh.max_satisfaction_weight(),
            Descriptor::Wpkh(ref wpkh) => wpkh.max_satisfaction_weight(),
            Descriptor::Wsh(ref wsh) => wsh.max_satisfaction_weight()?,
            Descriptor::Sh(ref sh) => sh.max_satisfaction_weight()?,
            Descriptor::LegacyCSFSCov(ref cov) => cov.max_satisfaction_weight()?,
            Descriptor::Tr(ref tr) => tr.max_satisfaction_weight()?,
            Descriptor::TrExt(ref tr) => tr.max_satisfaction_weight()?,
        };
        Ok(weight)
    }
}

impl<Pk: MiniscriptKey, Arg: ExtParam> Descriptor<Pk, CovenantExt<Arg>> {
    /// Create a new covenant descriptor
    // All extensions are supported in wsh descriptor
    pub fn new_cov_wsh(
        pk: Pk,
        ms: Miniscript<Pk, Segwitv0, CovenantExt<Arg>>,
    ) -> Result<Self, Error> {
        let cov = LegacyCSFSCov::new(pk, ms)?;
        Ok(Descriptor::LegacyCSFSCov(cov))
    }

    /// Tries to convert descriptor as a covenant descriptor
    pub fn as_cov(&self) -> Result<&LegacyCSFSCov<Pk, CovenantExt<Arg>>, Error> {
        if let Descriptor::LegacyCSFSCov(cov) = self {
            Ok(cov)
        } else {
            Err(Error::CovError(CovError::BadCovDescriptor))
        }
    }
}

impl<Pk: MiniscriptKey + ToPublicKey, Ext: Extension + ParseableExt> Descriptor<Pk, Ext> {
    ///
    /// Obtains the blinded address for this descriptor
    ///
    /// # Errors
    /// For raw/bare descriptors that don't have an address.
    //
    // Note: The address kept is kept without the blinder to avoid more conflicts with upstream
    pub fn blinded_address(
        &self,
        blinder: secp256k1_zkp::PublicKey,
        params: &'static elements::AddressParams,
    ) -> Result<elements::Address, Error>
    where
        Pk: ToPublicKey,
    {
        match *self {
            Descriptor::Bare(_) => Err(Error::BareDescriptorAddr),
            Descriptor::Pkh(ref pkh) => Ok(pkh.address(Some(blinder), params)),
            Descriptor::Wpkh(ref wpkh) => Ok(wpkh.address(Some(blinder), params)),
            Descriptor::Wsh(ref wsh) => Ok(wsh.address(Some(blinder), params)),
            Descriptor::Sh(ref sh) => Ok(sh.address(Some(blinder), params)),
            Descriptor::LegacyCSFSCov(ref cov) => Ok(cov.address(Some(blinder), params)),
            Descriptor::Tr(ref tr) => Ok(tr.address(Some(blinder), params)),
            Descriptor::TrExt(ref tr) => Ok(tr.address(Some(blinder), params)),
        }
    }

    /// Obtains an address for this descriptor. For blinding see [`Descriptor::blinded_address`]
    pub fn address(
        &self,
        params: &'static elements::AddressParams,
    ) -> Result<elements::Address, Error>
    where
        Pk: ToPublicKey,
    {
        match *self {
            Descriptor::Bare(_) => Err(Error::BareDescriptorAddr),
            Descriptor::Pkh(ref pkh) => Ok(pkh.address(None, params)),
            Descriptor::Wpkh(ref wpkh) => Ok(wpkh.address(None, params)),
            Descriptor::Wsh(ref wsh) => Ok(wsh.address(None, params)),
            Descriptor::Sh(ref sh) => Ok(sh.address(None, params)),
            Descriptor::LegacyCSFSCov(ref cov) => Ok(cov.address(None, params)),
            Descriptor::Tr(ref tr) => Ok(tr.address(None, params)),
            Descriptor::TrExt(ref tr) => Ok(tr.address(None, params)),
        }
    }

    /// Computes the scriptpubkey of the descriptor.
    pub fn script_pubkey(&self) -> Script {
        match *self {
            Descriptor::Bare(ref bare) => bare.script_pubkey(),
            Descriptor::Pkh(ref pkh) => pkh.script_pubkey(),
            Descriptor::Wpkh(ref wpkh) => wpkh.script_pubkey(),
            Descriptor::Wsh(ref wsh) => wsh.script_pubkey(),
            Descriptor::Sh(ref sh) => sh.script_pubkey(),
            Descriptor::LegacyCSFSCov(ref cov) => cov.script_pubkey(),
            Descriptor::Tr(ref tr) => tr.script_pubkey(),
            Descriptor::TrExt(ref tr) => tr.script_pubkey(),
        }
    }

    /// Computes the scriptSig that will be in place for an unsigned input
    /// spending an output with this descriptor. For pre-segwit descriptors,
    /// which use the scriptSig for signatures, this returns the empty script.
    ///
    /// This is used in Segwit transactions to produce an unsigned transaction
    /// whose txid will not change during signing (since only the witness data
    /// will change).
    pub fn unsigned_script_sig(&self) -> Script {
        match *self {
            Descriptor::Bare(_) => Script::new(),
            Descriptor::Pkh(_) => Script::new(),
            Descriptor::Wpkh(_) => Script::new(),
            Descriptor::Wsh(_) => Script::new(),
            Descriptor::Sh(ref sh) => sh.unsigned_script_sig(),
            Descriptor::LegacyCSFSCov(_) => Script::new(),
            Descriptor::Tr(_) => Script::new(),
            Descriptor::TrExt(_) => Script::new(),
        }
    }

    /// Computes the the underlying script before any hashing is done. For
    /// `Bare`, `Pkh` and `Wpkh` this is the scriptPubkey; for `ShWpkh` and `Sh`
    /// this is the redeemScript; for the others it is the witness script.
    ///
    /// # Errors
    /// If the descriptor is a taproot descriptor.
    pub fn explicit_script(&self) -> Result<Script, Error> {
        match *self {
            Descriptor::Bare(ref bare) => Ok(bare.script_pubkey()),
            Descriptor::Pkh(ref pkh) => Ok(pkh.script_pubkey()),
            Descriptor::Wpkh(ref wpkh) => Ok(wpkh.script_pubkey()),
            Descriptor::Wsh(ref wsh) => Ok(wsh.inner_script()),
            Descriptor::Sh(ref sh) => Ok(sh.inner_script()),
            Descriptor::Tr(_) => Err(Error::TrNoScriptCode),
            Descriptor::TrExt(_) => Err(Error::TrNoScriptCode),
            Descriptor::LegacyCSFSCov(ref cov) => Ok(cov.inner_script()),
        }
    }

    /// Computes the `scriptCode` of a transaction output.
    ///
    /// The `scriptCode` is the Script of the previous transaction output being
    /// serialized in the sighash when evaluating a `CHECKSIG` & co. OP code.
    ///
    /// # Errors
    /// If the descriptor is a taproot descriptor.
    pub fn script_code(&self) -> Result<Script, Error> {
        match *self {
            Descriptor::Bare(ref bare) => Ok(bare.ecdsa_sighash_script_code()),
            Descriptor::Pkh(ref pkh) => Ok(pkh.ecdsa_sighash_script_code()),
            Descriptor::Wpkh(ref wpkh) => Ok(wpkh.ecdsa_sighash_script_code()),
            Descriptor::Wsh(ref wsh) => Ok(wsh.ecdsa_sighash_script_code()),
            Descriptor::Sh(ref sh) => Ok(sh.ecdsa_sighash_script_code()),
            Descriptor::LegacyCSFSCov(ref cov) => Ok(cov.ecdsa_sighash_script_code()),
            Descriptor::Tr(_) => Err(Error::TrNoScriptCode),
            Descriptor::TrExt(_) => Err(Error::TrNoScriptCode),
        }
    }

    /// Returns satisfying non-malleable witness and scriptSig to spend an
    /// output controlled by the given descriptor if it possible to
    /// construct one using the satisfier S.
    pub fn get_satisfaction<S>(&self, satisfier: S) -> Result<(Vec<Vec<u8>>, Script), Error>
    where
        S: Satisfier<Pk>,
    {
        match *self {
            Descriptor::Bare(ref bare) => bare.get_satisfaction(satisfier),
            Descriptor::Pkh(ref pkh) => pkh.get_satisfaction(satisfier),
            Descriptor::Wpkh(ref wpkh) => wpkh.get_satisfaction(satisfier),
            Descriptor::Wsh(ref wsh) => wsh.get_satisfaction(satisfier),
            Descriptor::Sh(ref sh) => sh.get_satisfaction(satisfier),
            Descriptor::LegacyCSFSCov(ref cov) => cov.get_satisfaction(satisfier),
            Descriptor::Tr(ref tr) => tr.get_satisfaction(satisfier),
            Descriptor::TrExt(ref tr) => tr.get_satisfaction(satisfier),
        }
    }

    /// Returns a possilbly mallable satisfying non-malleable witness and scriptSig to spend an
    /// output controlled by the given descriptor if it possible to
    /// construct one using the satisfier S.
    pub fn get_satisfaction_mall<S>(&self, satisfier: S) -> Result<(Vec<Vec<u8>>, Script), Error>
    where
        S: Satisfier<Pk>,
    {
        match *self {
            Descriptor::Bare(ref bare) => bare.get_satisfaction_mall(satisfier),
            Descriptor::Pkh(ref pkh) => pkh.get_satisfaction_mall(satisfier),
            Descriptor::Wpkh(ref wpkh) => wpkh.get_satisfaction_mall(satisfier),
            Descriptor::Wsh(ref wsh) => wsh.get_satisfaction_mall(satisfier),
            Descriptor::Sh(ref sh) => sh.get_satisfaction_mall(satisfier),
            Descriptor::LegacyCSFSCov(ref cov) => cov.get_satisfaction_mall(satisfier),
            Descriptor::Tr(ref tr) => tr.get_satisfaction_mall(satisfier),
            Descriptor::TrExt(ref tr) => tr.get_satisfaction_mall(satisfier),
        }
    }

    /// Attempts to produce a non-malleable satisfying witness and scriptSig to spend an
    /// output controlled by the given descriptor; add the data to a given
    /// `TxIn` output.
    pub fn satisfy<S>(&self, txin: &mut TxIn, satisfier: S) -> Result<(), Error>
    where
        S: Satisfier<Pk>,
    {
        let (witness, script_sig) = self.get_satisfaction(satisfier)?;
        txin.witness.script_witness = witness;
        txin.script_sig = script_sig;
        Ok(())
    }
}

impl<P, Q, Ext> TranslatePk<P, Q> for Descriptor<P, Ext>
where
    P: MiniscriptKey,
    Q: MiniscriptKey,
    Ext: Extension,
{
    type Output = Descriptor<Q, Ext>;

    /// Converts a descriptor using abstract keys to one using specific keys.
    fn translate_pk<T, E>(&self, t: &mut T) -> Result<Self::Output, E>
    where
        T: Translator<P, Q, E>,
    {
        let desc = match *self {
            Descriptor::Bare(ref bare) => Descriptor::Bare(bare.translate_pk(t)?),
            Descriptor::Pkh(ref pk) => Descriptor::Pkh(pk.translate_pk(t)?),
            Descriptor::Wpkh(ref pk) => Descriptor::Wpkh(pk.translate_pk(t)?),
            Descriptor::Sh(ref sh) => Descriptor::Sh(sh.translate_pk(t)?),
            Descriptor::Wsh(ref wsh) => Descriptor::Wsh(wsh.translate_pk(t)?),
            Descriptor::Tr(ref tr) => Descriptor::Tr(tr.translate_pk(t)?),
            Descriptor::TrExt(ref tr) => Descriptor::TrExt(tr.translate_pk(t)?),
            Descriptor::LegacyCSFSCov(ref cov) => Descriptor::LegacyCSFSCov(cov.translate_pk(t)?),
        };
        Ok(desc)
    }
}

impl<PExt, QExt, Pk> TranslateExt<PExt, QExt> for Descriptor<Pk, PExt>
where
    PExt: Extension + TranslateExt<PExt, QExt, Output = QExt>,
    QExt: Extension,
    Pk: MiniscriptKey,
{
    type Output = Descriptor<Pk, QExt>;

    /// Converts a descriptor using abstract keys to one using specific keys.
    #[rustfmt::skip]
    fn translate_ext<T, E>(&self, t: &mut T) -> Result<Self::Output, E>
    where
        T: ExtTranslator<PExt, QExt, E>,
    {
        let desc = match *self {
            Descriptor::Bare(ref bare) => Descriptor::Bare(bare.clone()),
            Descriptor::Pkh(ref pk) => Descriptor::Pkh(pk.clone()),
            Descriptor::Wpkh(ref pk) => Descriptor::Wpkh(pk.clone()),
            Descriptor::Sh(ref sh) => Descriptor::Sh(sh.clone()),
            Descriptor::Wsh(ref wsh) => Descriptor::Wsh(wsh.clone()),
            Descriptor::Tr(ref tr) => Descriptor::Tr(tr.clone()),
            Descriptor::TrExt(ref tr) => Descriptor::TrExt(
                TranslateExt::<PExt, QExt>::translate_ext(tr, t)?,
            ),
            Descriptor::LegacyCSFSCov(ref cov) => {
                Descriptor::LegacyCSFSCov(TranslateExt::<PExt, QExt>::translate_ext(
                    cov, t,
                )?)
            }
        };
        Ok(desc)
    }
}

impl<Pk: MiniscriptKey, T: Extension> ForEachKey<Pk> for Descriptor<Pk, T> {
    fn for_each_key<'a, F: FnMut(&'a Pk) -> bool>(&'a self, pred: F) -> bool
    where
        Pk: 'a,
    {
        match *self {
            Descriptor::Bare(ref bare) => bare.for_each_key(pred),
            Descriptor::Pkh(ref pkh) => pkh.for_each_key(pred),
            Descriptor::Wpkh(ref wpkh) => wpkh.for_each_key(pred),
            Descriptor::Wsh(ref wsh) => wsh.for_each_key(pred),
            Descriptor::Sh(ref sh) => sh.for_each_key(pred),
            Descriptor::LegacyCSFSCov(ref cov) => cov.for_any_key(pred),
            Descriptor::Tr(ref tr) => tr.for_each_key(pred),
            Descriptor::TrExt(ref tr) => tr.for_each_key(pred),
        }
    }
}

impl<Ext: Extension + ParseableExt> Descriptor<DescriptorPublicKey, Ext> {
    /// Whether or not the descriptor has any wildcards
    #[deprecated(note = "use has_wildcards instead")]
    pub fn is_deriveable(&self) -> bool {
        self.has_wildcard()
    }

    /// Whether or not the descriptor has any wildcards i.e. `/*`.
    pub fn has_wildcard(&self) -> bool {
        self.for_any_key(|key| key.has_wildcard())
    }

    /// Replaces all wildcards (i.e. `/*`) in the descriptor with a particular derivation index,
    /// turning it into a *definite* descriptor.
    ///
    /// # Errors
    /// - If index ≥ 2^31
    pub fn at_derivation_index(
        &self,
        index: u32,
    ) -> Result<Descriptor<DefiniteDescriptorKey, Ext>, ConversionError> {
        struct Derivator(u32);

        impl Translator<DescriptorPublicKey, DefiniteDescriptorKey, ConversionError> for Derivator {
            fn pk(
                &mut self,
                pk: &DescriptorPublicKey,
            ) -> Result<DefiniteDescriptorKey, ConversionError> {
                pk.clone().at_derivation_index(self.0)
            }

            translate_hash_clone!(DescriptorPublicKey, DescriptorPublicKey, ConversionError);
        }
        self.translate_pk(&mut Derivator(index))
    }

    #[deprecated(note = "use at_derivation_index instead")]
    /// Deprecated name for [`Self::at_derivation_index`].
    pub fn derive(
        &self,
        index: u32,
    ) -> Result<Descriptor<DefiniteDescriptorKey, Ext>, ConversionError> {
        self.at_derivation_index(index)
    }

    /// Convert all the public keys in the descriptor to [`bitcoin::PublicKey`] by deriving them or
    /// otherwise converting them. All [`bitcoin::key::XOnlyPublicKey`]s are converted to by adding a
    /// default(0x02) y-coordinate.
    ///
    /// This is a shorthand for:
    ///
    /// ```
    /// # use elements_miniscript::{Descriptor, DescriptorPublicKey, bitcoin::secp256k1::Secp256k1};
    /// # use core::str::FromStr;
    /// # let descriptor = Descriptor::<DescriptorPublicKey>::from_str("eltr(xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/*)")
    ///     .expect("Valid ranged descriptor");
    /// # let index = 42;
    /// # let secp = Secp256k1::verification_only();
    /// let derived_descriptor = descriptor.at_derivation_index(index).unwrap().derived_descriptor(&secp).unwrap();
    /// # assert_eq!(descriptor.derived_descriptor(&secp, index).unwrap(), derived_descriptor);
    /// ```
    ///
    /// and is only here really here for backwards compatbility.
    /// See [`at_derivation_index`] and `[derived_descriptor`] for more documentation.
    ///
    /// [`at_derivation_index`]: Self::at_derivation_index
    /// [`derived_descriptor`]: crate::DerivedDescriptor::derived_descriptor
    ///
    /// # Errors
    ///
    /// This function will return an error if hardened derivation is attempted.
    pub fn derived_descriptor<C: secp256k1_zkp::Verification>(
        &self,
        secp: &secp256k1_zkp::Secp256k1<C>,
        index: u32,
    ) -> Result<Descriptor<bitcoin::PublicKey, Ext>, ConversionError> {
        self.at_derivation_index(index)?.derived_descriptor(secp)
    }

    /// Parse a descriptor that may contain secret keys
    ///
    /// Internally turns every secret key found into the corresponding public key and then returns a
    /// a descriptor that only contains public keys and a map to lookup the secret key given a public key.
    pub fn parse_descriptor<C: secp256k1_zkp::Signing>(
        secp: &secp256k1_zkp::Secp256k1<C>,
        s: &str,
    ) -> Result<(Descriptor<DescriptorPublicKey, Ext>, KeyMap), Error> {
        fn parse_key<C: secp256k1::Signing>(
            s: &str,
            key_map: &mut KeyMap,
            secp: &secp256k1::Secp256k1<C>,
        ) -> Result<DescriptorPublicKey, Error> {
            let (public_key, secret_key) = match DescriptorSecretKey::from_str(s) {
                Ok(sk) => (
                    sk.to_public(secp)
                        .map_err(|e| Error::Unexpected(e.to_string()))?,
                    Some(sk),
                ),
                Err(_) => (
                    DescriptorPublicKey::from_str(s)
                        .map_err(|e| Error::Unexpected(e.to_string()))?,
                    None,
                ),
            };

            if let Some(secret_key) = secret_key {
                key_map.insert(public_key.clone(), secret_key);
            }

            Ok(public_key)
        }

        let mut keymap_pk = KeyMapWrapper(HashMap::new(), secp);

        struct KeyMapWrapper<'a, C: secp256k1::Signing>(KeyMap, &'a secp256k1::Secp256k1<C>);

        impl<'a, C: secp256k1::Signing> Translator<String, DescriptorPublicKey, Error>
            for KeyMapWrapper<'a, C>
        {
            fn pk(&mut self, pk: &String) -> Result<DescriptorPublicKey, Error> {
                parse_key(pk, &mut self.0, self.1)
            }

            fn sha256(&mut self, sha256: &String) -> Result<sha256::Hash, Error> {
                let hash =
                    sha256::Hash::from_str(sha256).map_err(|e| Error::Unexpected(e.to_string()))?;
                Ok(hash)
            }

            fn hash256(&mut self, hash256: &String) -> Result<hash256::Hash, Error> {
                let hash = hash256::Hash::from_str(hash256)
                    .map_err(|e| Error::Unexpected(e.to_string()))?;
                Ok(hash)
            }

            fn ripemd160(&mut self, ripemd160: &String) -> Result<ripemd160::Hash, Error> {
                let hash = ripemd160::Hash::from_str(ripemd160)
                    .map_err(|e| Error::Unexpected(e.to_string()))?;
                Ok(hash)
            }

            fn hash160(&mut self, hash160: &String) -> Result<hash160::Hash, Error> {
                let hash = hash160::Hash::from_str(hash160)
                    .map_err(|e| Error::Unexpected(e.to_string()))?;
                Ok(hash)
            }
        }

        let descriptor = Descriptor::<String, Ext>::from_str(s)?;
        let descriptor = descriptor
            .translate_pk(&mut keymap_pk)
            .map_err(|e| Error::Unexpected(e.to_string()))?;

        Ok((descriptor, keymap_pk.0))
    }

    /// Serialize a descriptor to string with its secret keys
    pub fn to_string_with_secret(&self, key_map: &KeyMap) -> String {
        struct KeyMapLookUp<'a>(&'a KeyMap);

        impl<'a> Translator<DescriptorPublicKey, String, ()> for KeyMapLookUp<'a> {
            fn pk(&mut self, pk: &DescriptorPublicKey) -> Result<String, ()> {
                key_to_string(pk, self.0)
            }

            fn sha256(&mut self, sha256: &sha256::Hash) -> Result<String, ()> {
                Ok(sha256.to_string())
            }

            fn hash256(&mut self, hash256: &hash256::Hash) -> Result<String, ()> {
                Ok(hash256.to_string())
            }

            fn ripemd160(&mut self, ripemd160: &ripemd160::Hash) -> Result<String, ()> {
                Ok(ripemd160.to_string())
            }

            fn hash160(&mut self, hash160: &hash160::Hash) -> Result<String, ()> {
                Ok(hash160.to_string())
            }
        }

        fn key_to_string(pk: &DescriptorPublicKey, key_map: &KeyMap) -> Result<String, ()> {
            Ok(match key_map.get(pk) {
                Some(secret) => secret.to_string(),
                None => pk.to_string(),
            })
        }

        let descriptor = self
            .translate_pk(&mut KeyMapLookUp(key_map))
            .expect("Translation to string cannot fail");

        descriptor.to_string()
    }

    /// Whether this descriptor contains a key that has multiple derivation paths.
    pub fn is_multipath(&self) -> bool {
        self.for_any_key(DescriptorPublicKey::is_multipath)
    }

    /// Get as many descriptors as different paths in this descriptor.
    ///
    /// For multipath descriptors it will return as many descriptors as there is
    /// "parallel" paths. For regular descriptors it will just return itself.
    #[allow(clippy::blocks_in_if_conditions)]
    pub fn into_single_descriptors(self) -> Result<Vec<Self>, Error> {
        // All single-path descriptors contained in this descriptor.
        let mut descriptors = Vec::new();
        // We (ab)use `for_any_key` to gather the number of separate descriptors.
        if !self.for_any_key(|key| {
            // All multipath keys must have the same number of indexes at the "multi-index"
            // step. So we can return early if we already populated the vector.
            if !descriptors.is_empty() {
                return true;
            }

            match key {
                DescriptorPublicKey::Single(..) | DescriptorPublicKey::XPub(..) => false,
                DescriptorPublicKey::MultiXPub(xpub) => {
                    for _ in 0..xpub.derivation_paths.paths().len() {
                        descriptors.push(self.clone());
                    }
                    true
                }
            }
        }) {
            // If there is no multipath key, return early.
            return Ok(vec![self]);
        }
        assert!(!descriptors.is_empty());

        // Now, transform the multipath key of each descriptor into a single-key using each index.
        struct IndexChoser(usize);
        impl Translator<DescriptorPublicKey, DescriptorPublicKey, Error> for IndexChoser {
            fn pk(&mut self, pk: &DescriptorPublicKey) -> Result<DescriptorPublicKey, Error> {
                match pk {
                    DescriptorPublicKey::Single(..) | DescriptorPublicKey::XPub(..) => {
                        Ok(pk.clone())
                    }
                    DescriptorPublicKey::MultiXPub(_) => pk
                        .clone()
                        .into_single_keys()
                        .get(self.0)
                        .cloned()
                        .ok_or(Error::MultipathDescLenMismatch),
                }
            }
            translate_hash_clone!(DescriptorPublicKey, DescriptorPublicKey, Error);
        }

        for (i, desc) in descriptors.iter_mut().enumerate() {
            let mut index_choser = IndexChoser(i);
            *desc = desc.translate_pk(&mut index_choser)?;
        }

        Ok(descriptors)
    }
}

impl<Ext: Extension + ParseableExt> Descriptor<DescriptorPublicKey, Ext> {
    /// Utility method for deriving the descriptor at each index in a range to find one matching
    /// `script_pubkey`.
    ///
    /// If it finds a match then it returns the index it was derived at and the concrete
    /// descriptor at that index. If the descriptor is non-derivable then it will simply check the
    /// script pubkey against the descriptor and return it if it matches (in this case the index
    /// returned will be meaningless).
    pub fn find_derivation_index_for_spk<C: secp256k1_zkp::Verification>(
        &self,
        secp: &secp256k1_zkp::Secp256k1<C>,
        script_pubkey: &Script,
        range: Range<u32>,
    ) -> Result<Option<(u32, Descriptor<bitcoin::PublicKey, Ext>)>, ConversionError> {
        let range = if self.has_wildcard() { range } else { 0..1 };

        for i in range {
            let concrete = self.derived_descriptor(secp, i)?;
            if &concrete.script_pubkey() == script_pubkey {
                return Ok(Some((i, concrete)));
            }
        }

        Ok(None)
    }
}

impl<Pk: MiniscriptKey, Ext: Extension> Descriptor<Pk, Ext> {
    /// Whether this descriptor is a multipath descriptor that contains any 2 multipath keys
    /// with a different number of derivation paths.
    /// Such a descriptor is invalid according to BIP389.
    pub fn multipath_length_mismatch(&self) -> bool {
        // (Ab)use `for_each_key` to record the number of derivation paths a multipath key has.
        #[derive(PartialEq)]
        enum MultipathLenChecker {
            SinglePath,
            MultipathLen(usize),
            LenMismatch,
        }

        let mut checker = MultipathLenChecker::SinglePath;
        self.for_each_key(|key| {
            match key.num_der_paths() {
                0 | 1 => {}
                n => match checker {
                    MultipathLenChecker::SinglePath => {
                        checker = MultipathLenChecker::MultipathLen(n);
                    }
                    MultipathLenChecker::MultipathLen(len) => {
                        if len != n {
                            checker = MultipathLenChecker::LenMismatch;
                        }
                    }
                    MultipathLenChecker::LenMismatch => {}
                },
            }
            true
        });

        checker == MultipathLenChecker::LenMismatch
    }
}

impl<Ext: Extension> Descriptor<DefiniteDescriptorKey, Ext> {
    /// Convert all the public keys in the descriptor to [`bitcoin::PublicKey`] by deriving them or
    /// otherwise converting them. All [`bitcoin::key::XOnlyPublicKey`]s are converted to by adding a
    /// default(0x02) y-coordinate.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate elements_miniscript as miniscript;
    /// use miniscript::descriptor::{Descriptor, DescriptorPublicKey};
    /// use miniscript::bitcoin::secp256k1;
    /// use std::str::FromStr;
    ///
    /// // test from bip 86
    /// let secp = secp256k1::Secp256k1::verification_only();
    /// let descriptor = Descriptor::<DescriptorPublicKey>::from_str("eltr(xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/*)")
    ///     .expect("Valid ranged descriptor");
    /// let result = descriptor.at_derivation_index(0).unwrap().derived_descriptor(&secp).expect("Non-hardened derivation");
    /// assert_eq!(result.to_string(), "eltr(03cc8a4bc64d897bddc5fbc2f670f7a8ba0b386779106cf1223c6fc5d7cd6fc115)#hr5pt2wj");
    /// ```
    ///
    /// # Errors
    ///
    /// This function will return an error if hardened derivation is attempted.
    pub fn derived_descriptor<C: secp256k1::Verification>(
        &self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<Descriptor<bitcoin::PublicKey, Ext>, ConversionError> {
        struct Derivator<'a, C: secp256k1::Verification>(&'a secp256k1::Secp256k1<C>);

        impl<'a, C: secp256k1::Verification>
            Translator<DefiniteDescriptorKey, bitcoin::PublicKey, ConversionError>
            for Derivator<'a, C>
        {
            fn pk(
                &mut self,
                pk: &DefiniteDescriptorKey,
            ) -> Result<bitcoin::PublicKey, ConversionError> {
                pk.derive_public_key(self.0)
            }

            translate_hash_clone!(DefiniteDescriptorKey, bitcoin::PublicKey, ConversionError);
        }

        let derived = self.translate_pk(&mut Derivator(secp))?;
        Ok(derived)
    }
}

impl_from_tree!(
    ;T; Extension,
    Descriptor<Pk, T>,
    /// Parse an expression tree into a descriptor.
    fn from_tree(top: &expression::Tree) -> Result<Descriptor<Pk, T>, Error> {
        Ok(match (top.name, top.args.len() as u32) {
            ("elpkh", 1) => Descriptor::Pkh(Pkh::from_tree(top)?),
            ("elwpkh", 1) => Descriptor::Wpkh(Wpkh::from_tree(top)?),
            ("elsh", 1) => Descriptor::Sh(Sh::from_tree(top)?),
            ("elcovwsh", 2) => Descriptor::LegacyCSFSCov(LegacyCSFSCov::from_tree(top)?),
            ("elwsh", 1) => Descriptor::Wsh(Wsh::from_tree(top)?),
            ("eltr", _) => Descriptor::Tr(Tr::from_tree(top)?),
            _ => Descriptor::Bare(Bare::from_tree(top)?),
        })
    }
);

impl_from_str!(
    ;T; Extension,
    Descriptor<Pk, T>,
    type Err = Error;,
    fn from_str(s: &str) -> Result<Descriptor<Pk, T>, Error> {
        if !s.starts_with(ELMTS_STR) {
            return Err(Error::BadDescriptor(String::from(
                "Not an Elements Descriptor",
            )));
        }
        // tr tree parsing has special code
        // Tr::from_str will check the checksum
        // match "tr(" to handle more extensibly
        let desc = if s.starts_with(&format!("{}tr", ELMTS_STR)) {
            // First try parsing without extensions
            match Tr::<Pk, NoExt>::from_str(s) {
                Ok(tr) => Descriptor::Tr(tr),
                Err(_) => {
                    // Try parsing with extensions
                    let tr = Tr::<Pk, T>::from_str(s)?;
                    Descriptor::TrExt(tr)
                }
            }
        } else {
            let desc_str = verify_checksum(s)?;
            let top = expression::Tree::from_str(desc_str)?;
            expression::FromTree::from_tree(&top)?
        };

        if desc.multipath_length_mismatch() {
            return Err(Error::MultipathDescLenMismatch);
        }

        Ok(desc)
    }
);

impl<Pk: MiniscriptKey, T: Extension> fmt::Debug for Descriptor<Pk, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Descriptor::Bare(ref sub) => fmt::Debug::fmt(sub, f),
            Descriptor::Pkh(ref pkh) => fmt::Debug::fmt(pkh, f),
            Descriptor::Wpkh(ref wpkh) => fmt::Debug::fmt(wpkh, f),
            Descriptor::Sh(ref sub) => fmt::Debug::fmt(sub, f),
            Descriptor::Wsh(ref sub) => fmt::Debug::fmt(sub, f),
            Descriptor::Tr(ref tr) => fmt::Debug::fmt(tr, f),
            Descriptor::TrExt(ref tr) => fmt::Debug::fmt(tr, f),
            Descriptor::LegacyCSFSCov(ref cov) => fmt::Debug::fmt(cov, f),
        }
    }
}

impl<Pk: MiniscriptKey, T: Extension> fmt::Display for Descriptor<Pk, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Descriptor::Bare(ref sub) => fmt::Display::fmt(sub, f),
            Descriptor::Pkh(ref pkh) => fmt::Display::fmt(pkh, f),
            Descriptor::Wpkh(ref wpkh) => fmt::Display::fmt(wpkh, f),
            Descriptor::Sh(ref sub) => fmt::Display::fmt(sub, f),
            Descriptor::Wsh(ref sub) => fmt::Display::fmt(sub, f),
            Descriptor::Tr(ref tr) => fmt::Display::fmt(tr, f),
            Descriptor::TrExt(ref tr) => fmt::Display::fmt(tr, f),
            Descriptor::LegacyCSFSCov(ref cov) => fmt::Display::fmt(cov, f),
        }
    }
}

serde_string_impl_pk!(Descriptor, "a script descriptor", T; Extension);

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use bitcoin;
    use bitcoin::{bip32, PublicKey};
    use elements::hashes::{hash160, sha256};
    use elements::hex::{FromHex, ToHex};
    use elements::opcodes::all::{OP_CLTV, OP_CSV};
    use elements::script::Instruction;
    use elements::{opcodes, script, Sequence};

    use super::checksum::desc_checksum;
    use super::tr::Tr;
    use super::*;
    use crate::descriptor::key::Wildcard;
    use crate::descriptor::{DescriptorPublicKey, DescriptorXKey};
    use crate::miniscript::satisfy::ElementsSig;
    #[cfg(feature = "compiler")]
    use crate::policy;
    use crate::{hex_script, Descriptor, Error, Miniscript, NoExt, Satisfier};

    type StdDescriptor = Descriptor<PublicKey, CovenantExt<CovExtArgs>>;
    const TEST_PK: &str =
        "elpk(020000000000000000000000000000000000000000000000000000000000000002)";

    fn roundtrip_descriptor(s: &str) {
        let desc = Descriptor::<String>::from_str(s).unwrap();
        let output = desc.to_string();
        let normalize_aliases = s.replace("c:pk_k(", "pk(").replace("c:pk_h(", "pkh(");
        assert_eq!(
            format!(
                "{}#{}",
                &normalize_aliases,
                desc_checksum(&normalize_aliases).unwrap()
            ),
            output
        );
    }

    // helper function to create elements txin from scriptsig and witness
    fn elements_txin(script_sig: Script, witness: Vec<Vec<u8>>) -> elements::TxIn {
        let txin_witness = elements::TxInWitness {
            script_witness: witness,
            ..Default::default()
        };
        elements::TxIn {
            previous_output: elements::OutPoint::default(),
            script_sig,
            sequence: Sequence::from_height(100),
            is_pegin: false,
            asset_issuance: elements::AssetIssuance::default(),
            witness: txin_witness,
        }
    }

    #[test]
    fn desc_rtt_tests() {
        roundtrip_descriptor("elc:pk_k()");
        roundtrip_descriptor("elwsh(pk())");
        roundtrip_descriptor("elwsh(c:pk_k())");
        roundtrip_descriptor("elc:pk_h()");
    }
    #[test]
    fn parse_descriptor() {
        StdDescriptor::from_str("(").unwrap_err();
        StdDescriptor::from_str("(x()").unwrap_err();
        StdDescriptor::from_str("(\u{7f}()3").unwrap_err();
        StdDescriptor::from_str("pk()").unwrap_err();
        StdDescriptor::from_str("nl:0").unwrap_err(); //issue 63
        let compressed_pk = "02be5645686309c6e6736dbd93940707cc9143d3cf29f1b877ff340e2cb2d259cf";
        assert_eq!(
            StdDescriptor::from_str("elsh(sortedmulti)")
                .unwrap_err()
                .to_string(),
            "unexpected «no arguments given for sortedmulti»"
        ); //issue 202
        assert_eq!(
            StdDescriptor::from_str(&format!("elsh(sortedmulti(2,{}))", compressed_pk))
                .unwrap_err()
                .to_string(),
            "unexpected «higher threshold than there were keys in sortedmulti»"
        ); //issue 202

        StdDescriptor::from_str(TEST_PK).unwrap();
        // fuzzer
        StdDescriptor::from_str("slip77").unwrap_err();

        let uncompressed_pk =
        "0414fc03b8df87cd7b872996810db8458d61da8448e531569c8517b469a119d267be5645686309c6e6736dbd93940707cc9143d3cf29f1b877ff340e2cb2d259cf";

        // Context tests
        StdDescriptor::from_str(&format!("elpk({})", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("elpkh({})", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("elsh(pk({}))", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("elwpkh({})", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("elsh(wpkh({}))", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("elwsh(pk{})", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("elsh(wsh(pk{}))", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!(
            "elor_i(pk({}),pk({}))",
            uncompressed_pk, uncompressed_pk
        ))
        .unwrap_err();
    }

    #[test]
    pub fn script_pubkey() {
        let bare = StdDescriptor::from_str(
            "elmulti(1,020000000000000000000000000000000000000000000000000000000000000002)",
        )
        .unwrap();
        assert_eq!(
            bare.script_pubkey(),
            hex_script(
                "512102000000000000000000000000000000000000000000000000000000000000000251ae"
            )
        );
        assert_eq!(
            bare.address(&elements::AddressParams::ELEMENTS)
                .unwrap_err()
                .to_string(),
            "Bare descriptors don't have address"
        );

        let pk = StdDescriptor::from_str(TEST_PK).unwrap();
        assert_eq!(
            pk.script_pubkey(),
            elements::Script::from(vec![
                0x21, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xac,
            ])
        );

        let pkh = StdDescriptor::from_str(
            "elpkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )",
        )
        .unwrap();
        assert_eq!(
            pkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_DUP)
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_str("84e9ed95a38613f0527ff685a9928abe2d4754d4",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUALVERIFY)
                .push_opcode(opcodes::all::OP_CHECKSIG)
                .into_script()
        );
        assert_eq!(
            pkh.address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "2dmYXpSu8YP6aLcJYhHfB1C19mdzSx2GPB9"
        );

        let wpkh = StdDescriptor::from_str(
            "elwpkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )",
        )
        .unwrap();
        assert_eq!(
            wpkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_PUSHBYTES_0)
                .push_slice(
                    &hash160::Hash::from_str("84e9ed95a38613f0527ff685a9928abe2d4754d4",).unwrap()
                        [..]
                )
                .into_script()
        );
        assert_eq!(
            wpkh.address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "ert1qsn57m9drscflq5nl76z6ny52hck5w4x57m69k3"
        );

        let shwpkh = StdDescriptor::from_str(
            "elsh(wpkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            shwpkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_str("f1c3b9a431134cb90a500ec06e0067cfa9b8bba7",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            shwpkh
                .address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "XZPaAbg6M83Fq5NqvbEGZ5kwy9RKSTke2s"
        );

        let sh = StdDescriptor::from_str(
            "elsh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            sh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_str("aa5282151694d3f2f32ace7d00ad38f927a33ac8",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            sh.address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "XSspZXDJu2XVh8AKC7qF3L7x79Qy67JhQb"
        );

        let wsh = StdDescriptor::from_str(
            "elwsh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            wsh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_PUSHBYTES_0)
                .push_slice(
                    &sha256::Hash::from_str(
                        "\
                         f9379edc8983152dc781747830075bd5\
                         3896e4b0ce5bff73777fd77d124ba085\
                         "
                    )
                    .unwrap()[..]
                )
                .into_script()
        );
        assert_eq!(
            wsh.address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "ert1qlymeahyfsv2jm3upw3urqp6m65ufde9seedl7umh0lth6yjt5zzsan9u2t"
        );

        let shwsh = StdDescriptor::from_str(
            "elsh(wsh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )))",
        )
        .unwrap();
        assert_eq!(
            shwsh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_str("4bec5d7feeed99e1d0a23fe32a4afe126a7ff07e",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            shwsh
                .address(&elements::AddressParams::ELEMENTS,)
                .unwrap()
                .to_string(),
            "XJGggUb965TvGF2VCxp9EQGmZTxMeDjwQQ"
        );
    }

    #[test]
    fn satisfy() {
        let secp = secp256k1_zkp::Secp256k1::new();
        let sk =
            secp256k1_zkp::SecretKey::from_slice(&b"sally was a secret key, she said"[..]).unwrap();
        let pk = bitcoin::PublicKey {
            inner: secp256k1_zkp::PublicKey::from_secret_key(&secp, &sk),
            compressed: true,
        };
        let msg = secp256k1_zkp::Message::from_slice(&b"michael was a message, amusingly"[..])
            .expect("32 bytes");
        let sig = secp.sign_ecdsa(&msg, &sk);
        let mut sigser = sig.serialize_der().to_vec();
        sigser.push(0x01); // sighash_all

        struct SimpleSat {
            sig: secp256k1_zkp::ecdsa::Signature,
            pk: bitcoin::PublicKey,
        }

        impl Satisfier<bitcoin::PublicKey> for SimpleSat {
            fn lookup_ecdsa_sig(&self, pk: &bitcoin::PublicKey) -> Option<ElementsSig> {
                if *pk == self.pk {
                    Some((self.sig, elements::EcdsaSighashType::All))
                } else {
                    None
                }
            }
        }

        let satisfier = SimpleSat { sig, pk };
        let ms = ms_str!("c:pk_k({})", pk);

        let mut txin = elements::TxIn {
            previous_output: elements::OutPoint::default(),
            script_sig: Script::new(),
            sequence: Sequence::from_height(100),
            is_pegin: false,
            asset_issuance: elements::AssetIssuance::default(),
            witness: elements::TxInWitness::default(),
        };
        let bare: Descriptor<_, NoExt> = Descriptor::new_bare(ms).unwrap();

        bare.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            elements_txin(
                script::Builder::new().push_slice(&sigser[..]).into_script(),
                vec![]
            ),
        );
        assert_eq!(bare.unsigned_script_sig(), elements::Script::new());

        let pkh: Descriptor<_, NoExt> = Descriptor::new_pkh(pk);
        pkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            elements_txin(
                script::Builder::new()
                    .push_slice(&sigser[..])
                    .push_key(&pk)
                    .into_script(),
                vec![]
            )
        );
        assert_eq!(pkh.unsigned_script_sig(), elements::Script::new());

        let wpkh: Descriptor<_, NoExt> = Descriptor::new_wpkh(pk).unwrap();
        wpkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            elements_txin(Script::new(), vec![sigser.clone(), pk.to_bytes(),])
        );
        assert_eq!(wpkh.unsigned_script_sig(), elements::Script::new());

        let shwpkh: Descriptor<_, NoExt> = Descriptor::new_sh_wpkh(pk).unwrap();
        shwpkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        let redeem_script = script::Builder::new()
            .push_opcode(opcodes::all::OP_PUSHBYTES_0)
            .push_slice(
                &hash160::Hash::from_str("d1b2a1faf62e73460af885c687dee3b7189cd8ab").unwrap()[..],
            )
            .into_script();
        let expected_ssig = script::Builder::new()
            .push_slice(&redeem_script[..])
            .into_script();
        assert_eq!(
            txin,
            elements_txin(expected_ssig.clone(), vec![sigser.clone(), pk.to_bytes()])
        );
        assert_eq!(shwpkh.unsigned_script_sig(), expected_ssig);

        let ms = ms_str!("c:pk_k({})", pk);
        let sh: Descriptor<_, NoExt> = Descriptor::new_sh(ms.clone()).unwrap();
        sh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        let expected_ssig = script::Builder::new()
            .push_slice(&sigser[..])
            .push_slice(&ms.encode()[..])
            .into_script();
        assert_eq!(txin, elements_txin(expected_ssig, vec![]));
        assert_eq!(sh.unsigned_script_sig(), Script::new());

        let ms = ms_str!("c:pk_k({})", pk);

        let wsh: Descriptor<_, NoExt> = Descriptor::new_wsh(ms.clone()).unwrap();
        wsh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            elements_txin(
                Script::new(),
                vec![sigser.clone(), ms.encode().into_bytes()]
            )
        );
        assert_eq!(wsh.unsigned_script_sig(), Script::new());

        let shwsh: Descriptor<_, NoExt> = Descriptor::new_sh_wsh(ms.clone()).unwrap();
        shwsh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        let expected_ssig = script::Builder::new()
            .push_slice(&ms.encode().to_v0_p2wsh()[..])
            .into_script();
        assert_eq!(
            txin,
            elements_txin(
                expected_ssig.clone(),
                vec![sigser.clone(), ms.encode().into_bytes(),]
            )
        );
        assert_eq!(shwsh.unsigned_script_sig(), expected_ssig);
    }

    #[test]
    fn after_is_cltv() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("elwsh(after(1000))").unwrap();
        let script = descriptor.explicit_script().unwrap();

        let actual_instructions: Vec<_> = script.instructions().collect();
        let check = actual_instructions.last().unwrap();

        assert_eq!(check, &Ok(Instruction::Op(OP_CLTV)))
    }

    #[test]
    fn older_is_csv() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("elwsh(older(1000))").unwrap();
        let script = descriptor.explicit_script().unwrap();

        let actual_instructions: Vec<_> = script.instructions().collect();
        let check = actual_instructions.last().unwrap();

        assert_eq!(check, &Ok(Instruction::Op(OP_CSV)))
    }

    #[test]
    fn tr_roundtrip_key() {
        let script = Tr::<String>::from_str("eltr()").unwrap().to_string();
        assert_eq!(script, format!("eltr()#sux3r82e"))
    }

    #[test]
    fn tr_roundtrip_script() {
        let descriptor = Tr::<String>::from_str("eltr(,{pk(),pk()})")
            .unwrap()
            .to_string();

        assert_eq!(descriptor, "eltr(,{pk(),pk()})#lxgcxh02");

        let descriptor = Descriptor::<String>::from_str("eltr(A,{pk(B),pk(C)})")
            .unwrap()
            .to_string();
        assert_eq!(descriptor, "eltr(A,{pk(B),pk(C)})#cx98s50f");
    }

    #[test]
    fn tr_roundtrip_tree() {
        let p1 = "020000000000000000000000000000000000000000000000000000000000000001";
        let p2 = "020000000000000000000000000000000000000000000000000000000000000002";
        let p3 = "020000000000000000000000000000000000000000000000000000000000000003";
        let p4 = "020000000000000000000000000000000000000000000000000000000000000004";
        let p5 = "03f8551772d66557da28c1de858124f365a8eb30ce6ad79c10e0f4c546d0ab0f82";
        let descriptor = Tr::<PublicKey>::from_str(&format!(
            "eltr({},{{pk({}),{{pk({}),or_d(pk({}),pkh({}))}}}})",
            p1, p2, p3, p4, p5
        ))
        .unwrap()
        .to_string();

        // p5.to_pubkeyhash() = 516ca378e588a7ed71336147e2a72848b20aca1a
        assert_eq!(
            descriptor,
            format!(
                "eltr({},{{pk({}),{{pk({}),or_d(pk({}),pkh({}))}}}})#y9kzzx3w",
                p1, p2, p3, p4, p5
            )
        )
    }

    #[test]
    fn tr_script_pubkey() {
        let key = Descriptor::<bitcoin::PublicKey>::from_str(
            "eltr(02e20e746af365e86647826397ba1c0e0d5cb685752976fe2f326ab76bdc4d6ee9)",
        )
        .unwrap();
        assert_eq!(
            key.script_pubkey().to_hex(),
            "51203f48e7c6203a75722733e3d9d06638da38d946066159c64684caf1622b2b0e33"
        )
    }

    #[test]
    fn roundtrip_tests() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("elmulti");
        assert_eq!(
            descriptor.unwrap_err().to_string(),
            "unexpected «no arguments given»"
        )
    }

    #[test]
    fn empty_thresh() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("elthresh");
        assert_eq!(
            descriptor.unwrap_err().to_string(),
            "unexpected «no arguments given»"
        )
    }

    #[test]
    fn witness_stack_for_andv_is_arranged_in_correct_order() {
        // arrange
        let a = bitcoin::PublicKey::from_str(
            "02937402303919b3a2ee5edd5009f4236f069bf75667b8e6ecf8e5464e20116a0e",
        )
        .unwrap();
        let sig_a = secp256k1_zkp::ecdsa::Signature::from_str("3045022100a7acc3719e9559a59d60d7b2837f9842df30e7edcd754e63227e6168cec72c5d022066c2feba4671c3d99ea75d9976b4da6c86968dbf3bab47b1061e7a1966b1778c").unwrap();

        let b = bitcoin::PublicKey::from_str(
            "02eb64639a17f7334bb5a1a3aad857d6fec65faef439db3de72f85c88bc2906ad3",
        )
        .unwrap();
        let sig_b = secp256k1_zkp::ecdsa::Signature::from_str("3044022075b7b65a7e6cd386132c5883c9db15f9a849a0f32bc680e9986398879a57c276022056d94d12255a4424f51c700ac75122cb354895c9f2f88f0cbb47ba05c9c589ba").unwrap();

        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str(&format!(
            "elwsh(and_v(v:pk({A}),pk({B})))",
            A = a,
            B = b
        ))
        .unwrap();

        let mut txin = elements_txin(Script::new(), vec![]);
        let satisfier = {
            let mut satisfier = HashMap::with_capacity(2);

            satisfier.insert(a, (sig_a, ::elements::EcdsaSighashType::All));
            satisfier.insert(b, (sig_b, ::elements::EcdsaSighashType::All));

            satisfier
        };

        // act
        descriptor.satisfy(&mut txin, &satisfier).unwrap();

        // assert
        let witness0 = &txin.witness.script_witness[0];
        let witness1 = &txin.witness.script_witness[1];

        let sig0 =
            secp256k1_zkp::ecdsa::Signature::from_der(&witness0[..witness0.len() - 1]).unwrap();
        let sig1 =
            secp256k1_zkp::ecdsa::Signature::from_der(&witness1[..witness1.len() - 1]).unwrap();

        // why are we asserting this way?
        // The witness stack is evaluated from top to bottom. Given an `and` instruction, the left arm of the and is going to evaluate first,
        // meaning the next witness element (on a three element stack, that is the middle one) needs to be the signature for the left side of the `and`.
        // The left side of the `and` performs a CHECKSIG against public key `a` so `sig1` needs to be `sig_a` and `sig0` needs to be `sig_b`.
        assert_eq!(sig1, sig_a);
        assert_eq!(sig0, sig_b);
    }

    #[test]
    fn test_scriptcode() {
        // P2WPKH (from bip143 test vectors)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "elwpkh(025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee6357)",
        )
        .unwrap();
        assert_eq!(
            *descriptor.script_code().unwrap().as_bytes(),
            Vec::<u8>::from_hex("76a9141d0f172a0ecb48aee1be1f2687d2963ae33f71a188ac").unwrap()[..]
        );

        // P2SH-P2WPKH (from bip143 test vectors)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "elsh(wpkh(03ad1d8e89212f0b92c74d23bb710c00662ad1470198ac48c43f7d6f93a2a26873))",
        )
        .unwrap();
        assert_eq!(
            *descriptor.script_code().unwrap().as_bytes(),
            Vec::<u8>::from_hex("76a91479091972186c449eb1ded22b78e40d009bdf008988ac").unwrap()[..]
        );

        // P2WSH (from bitcoind's `createmultisig`)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "elwsh(multi(2,03789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd,03dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a61626))",
        )
        .unwrap();
        assert_eq!(
            *descriptor
                .script_code().unwrap()
                .as_bytes(),
            Vec::<u8>::from_hex("522103789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd2103dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a6162652ae").unwrap()[..]
        );

        // P2SH-P2WSH (from bitcoind's `createmultisig`)
        let descriptor = Descriptor::<PublicKey>::from_str("elsh(wsh(multi(2,03789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd,03dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a61626)))").unwrap();
        assert_eq!(
            *descriptor
                .script_code().unwrap()
                .as_bytes(),
            Vec::<u8>::from_hex("522103789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd2103dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a6162652ae")
                .unwrap()[..]
        );
    }

    #[test]
    fn parse_descriptor_key() {
        // With a wildcard
        let key = "[78412e3a/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: Some((
                bip32::Fingerprint::from([0x78, 0x41, 0x2e, 0x3a]),
                (&[
                    bip32::ChildNumber::from_hardened_idx(44).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                ][..])
                .into(),
            )),
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            wildcard: Wildcard::Unhardened,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Without origin
        let key = "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            wildcard: Wildcard::None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Testnet tpub
        let key = "tpubD6NzVbkrYhZ4YqYr3amYH15zjxHvBkUUeadieW8AxTZC7aY2L8aPSk3tpW6yW1QnWzXAB7zoiaNMfwXPPz9S68ZCV4yWvkVXjdeksLskCed/1";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("tpubD6NzVbkrYhZ4YqYr3amYH15zjxHvBkUUeadieW8AxTZC7aY2L8aPSk3tpW6yW1QnWzXAB7zoiaNMfwXPPz9S68ZCV4yWvkVXjdeksLskCed").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            wildcard: Wildcard::None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Without derivation path
        let key = "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: bip32::DerivationPath::from(&[][..]),
            wildcard: Wildcard::None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw (compressed) pubkey
        let key = "03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8";
        let expected = DescriptorPublicKey::Single(SinglePub {
            key: SinglePubKey::FullKey(
                bitcoin::PublicKey::from_str(
                    "03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8",
                )
                .unwrap(),
            ),
            origin: None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw (uncompressed) pubkey
        let key = "04f5eeb2b10c944c6b9fbcfff94c35bdeecd93df977882babc7f3a2cf7f5c81d3b09a68db7f0e04f21de5d4230e75e6dbe7ad16eefe0d4325a62067dc6f369446a";
        let expected = DescriptorPublicKey::Single(SinglePub {
            key: SinglePubKey::FullKey(bitcoin::PublicKey::from_str(
                "04f5eeb2b10c944c6b9fbcfff94c35bdeecd93df977882babc7f3a2cf7f5c81d3b09a68db7f0e04f21de5d4230e75e6dbe7ad16eefe0d4325a62067dc6f369446a",
            )
            .unwrap()),
            origin: None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw pubkey with origin
        let desc =
            "[78412e3a/0'/42/0']0231c7d3fc85c148717848033ce276ae2b464a4e2c367ed33886cc428b8af48ff8";
        let expected = DescriptorPublicKey::Single(SinglePub {
            key: SinglePubKey::FullKey(
                bitcoin::PublicKey::from_str(
                    "0231c7d3fc85c148717848033ce276ae2b464a4e2c367ed33886cc428b8af48ff8",
                )
                .unwrap(),
            ),
            origin: Some((
                bip32::Fingerprint::from([0x78, 0x41, 0x2e, 0x3a]),
                (&[
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                    bip32::ChildNumber::from_normal_idx(42).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                ][..])
                    .into(),
            )),
        });
        assert_eq!(expected, desc.parse().expect("Parsing desc"));
        assert_eq!(format!("{}", expected), desc);
    }

    #[test]
    fn test_sortedmulti() {
        fn _test_sortedmulti(raw_desc_one: &str, raw_desc_two: &str, raw_addr_expected: &str) {
            let secp_ctx = secp256k1_zkp::Secp256k1::verification_only();
            let index = 5;

            // Parse descriptor
            let desc_one = Descriptor::<DescriptorPublicKey>::from_str(raw_desc_one).unwrap();
            let desc_two = Descriptor::<DescriptorPublicKey>::from_str(raw_desc_two).unwrap();

            // Same string formatting
            assert_eq!(desc_one.to_string(), raw_desc_one);
            assert_eq!(desc_two.to_string(), raw_desc_two);

            // Same address
            let addr_one = desc_one
                .at_derivation_index(index)
                .unwrap()
                .derived_descriptor(&secp_ctx)
                .unwrap()
                .address(&elements::AddressParams::ELEMENTS)
                .unwrap();
            let addr_two = desc_two
                .at_derivation_index(index)
                .unwrap()
                .derived_descriptor(&secp_ctx)
                .unwrap()
                .address(&elements::AddressParams::ELEMENTS)
                .unwrap();
            let addr_expected = elements::Address::from_str(raw_addr_expected).unwrap();
            assert_eq!(addr_one, addr_expected);
            assert_eq!(addr_two, addr_expected);
        }

        // P2SH and pubkeys
        _test_sortedmulti(
            "elsh(sortedmulti(1,03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556,0250863ad64a87ae8a2fe83c1af1a8403cb53f53e486d8511dad8a04887e5b2352))#tse3qz98",
            "elsh(sortedmulti(1,0250863ad64a87ae8a2fe83c1af1a8403cb53f53e486d8511dad8a04887e5b2352,03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556))#ptnf05qc",
            "XUDXJZnP2GXsKRKdxSLKzJM1iZ4gbbyrGh",
        );

        // P2WSH and single-xpub descriptor
        _test_sortedmulti(
            "elwsh(sortedmulti(1,xpub661MyMwAqRbcFW31YEwpkMuc5THy2PSt5bDMsktWQcFF8syAmRUapSCGu8ED9W6oDMSgv6Zz8idoc4a6mr8BDzTJY47LJhkJ8UB7WEGuduB,xpub69H7F5d8KSRgmmdJg2KhpAK8SR3DjMwAdkxj3ZuxV27CprR9LgpeyGmXUbC6wb7ERfvrnKZjXoUmmDznezpbZb7ap6r1D3tgFxHmwMkQTPH))#a8h2v83d",
            "elwsh(sortedmulti(1,xpub69H7F5d8KSRgmmdJg2KhpAK8SR3DjMwAdkxj3ZuxV27CprR9LgpeyGmXUbC6wb7ERfvrnKZjXoUmmDznezpbZb7ap6r1D3tgFxHmwMkQTPH,xpub661MyMwAqRbcFW31YEwpkMuc5THy2PSt5bDMsktWQcFF8syAmRUapSCGu8ED9W6oDMSgv6Zz8idoc4a6mr8BDzTJY47LJhkJ8UB7WEGuduB))#qfcn7ujk",
            "ert1qpq2cfgz5lktxzr5zqv7nrzz46hsvq3492ump9pz8rzcl8wqtwqcs2yqnuv",
        );

        // P2WSH-P2SH and ranged descriptor
        _test_sortedmulti(
            "elsh(wsh(sortedmulti(1,xpub661MyMwAqRbcFW31YEwpkMuc5THy2PSt5bDMsktWQcFF8syAmRUapSCGu8ED9W6oDMSgv6Zz8idoc4a6mr8BDzTJY47LJhkJ8UB7WEGuduB/1/0/*,xpub69H7F5d8KSRgmmdJg2KhpAK8SR3DjMwAdkxj3ZuxV27CprR9LgpeyGmXUbC6wb7ERfvrnKZjXoUmmDznezpbZb7ap6r1D3tgFxHmwMkQTPH/0/0/*)))#l7qy253t",
            "elsh(wsh(sortedmulti(1,xpub69H7F5d8KSRgmmdJg2KhpAK8SR3DjMwAdkxj3ZuxV27CprR9LgpeyGmXUbC6wb7ERfvrnKZjXoUmmDznezpbZb7ap6r1D3tgFxHmwMkQTPH/0/0/*,xpub661MyMwAqRbcFW31YEwpkMuc5THy2PSt5bDMsktWQcFF8syAmRUapSCGu8ED9W6oDMSgv6Zz8idoc4a6mr8BDzTJY47LJhkJ8UB7WEGuduB/1/0/*)))#0gpee5cl",
            "XBkDY63XnRTz6BbwzJi3ifGhBwLTomEzkq",
        );
    }

    #[test]
    fn test_parse_descriptor() {
        let secp = &secp256k1_zkp::Secp256k1::signing_only();
        let (descriptor, key_map) = Descriptor::<_, NoExt>::parse_descriptor(secp, "elwpkh(tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/44'/0'/0'/0/*)").unwrap();
        assert_eq!(descriptor.to_string(), "elwpkh([2cbe2a6d/44'/0'/0']tpubDCvNhURocXGZsLNqWcqD3syHTqPXrMSTwi8feKVwAcpi29oYKsDD3Vex7x2TDneKMVN23RbLprfxB69v94iYqdaYHsVz3kPR37NQXeqouVz/0/*)#pznhhta9");
        assert_eq!(key_map.len(), 1);

        // https://github.com/bitcoin/bitcoin/blob/7ae86b3c6845873ca96650fc69beb4ae5285c801/src/test/descriptor_tests.cpp#L355-L360
        macro_rules! check_invalid_checksum {
            ($secp: ident,$($desc: expr),*) => {
                $(
                    match Descriptor::<_, NoExt>::parse_descriptor($secp, $desc) {
                        Err(Error::BadDescriptor(_)) => {},
                        Err(e) => panic!("Expected bad checksum for {}, got '{}'", $desc, e),
                        _ => panic!("Invalid checksum treated as valid: {}", $desc),
                    };
                )*
            };
        }
        check_invalid_checksum!(secp,
            "elsh(multi(2,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))#",
            "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#",
            "elsh(multi(2,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))#ggrsrxf",
            "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#tjg09x5tq",
            "elsh(multi(2,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))#ggrsrxf",
            "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#tjg09x5",
            "elsh(multi(3,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))#ggrsrxfy",
            "elsh(multi(3,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#tjg09x5t",
            "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#tjq09x4t",
            "elsh(multi(2,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))##ggssrxfy",
            "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))##tjq09x4t"
        );

        Descriptor::<_, NoExt>::parse_descriptor(secp, "elsh(multi(2,[00000000/111'/222]xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc,xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L/0))#9s2ngs7u").expect("Valid descriptor with checksum");
        Descriptor::<_, NoExt>::parse_descriptor(secp, "elsh(multi(2,[00000000/111'/222]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL,xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y/0))#uklept69").expect("Valid descriptor with checksum");
    }

    #[test]
    #[cfg(feature = "compiler")]
    fn parse_and_derive() {
        let descriptor_str = "thresh(2,\
pk([d34db33f/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*),\
pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1),\
pk(03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8))";
        let policy: policy::concrete::Policy<DescriptorPublicKey> = descriptor_str.parse().unwrap();
        let descriptor = Descriptor::<_, NoExt>::new_sh(policy.compile().unwrap()).unwrap();
        let definite_descriptor = descriptor.at_derivation_index(42).unwrap();

        let res_descriptor_str = "thresh(2,\
pk([d34db33f/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/42),\
pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1),\
pk(03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8))";
        let res_policy: policy::concrete::Policy<DescriptorPublicKey> =
            res_descriptor_str.parse().unwrap();
        let res_descriptor =
            Descriptor::<DescriptorPublicKey, NoExt>::new_sh(res_policy.compile().unwrap())
                .unwrap();

        assert_eq!(res_descriptor.to_string(), definite_descriptor.to_string());
    }

    #[test]
    fn parse_with_secrets() {
        let secp = &secp256k1_zkp::Secp256k1::signing_only();
        let descriptor_str = "elwpkh(xprv9s21ZrQH143K4CTb63EaMxja1YiTnSEWKMbn23uoEnAzxjdUJRQkazCAtzxGm4LSoTSVTptoV9RbchnKPW9HxKtZumdyxyikZFDLhogJ5Uj/44'/0'/0'/0/*)#xldrpn5u";
        let (descriptor, keymap) =
            Descriptor::<DescriptorPublicKey>::parse_descriptor(secp, descriptor_str).unwrap();

        let expected = "elwpkh([a12b02f4/44'/0'/0']xpub6BzhLAQUDcBUfHRQHZxDF2AbcJqp4Kaeq6bzJpXrjrWuK26ymTFwkEFbxPra2bJ7yeZKbDjfDeFwxe93JMqpo5SsPJH6dZdvV9kMzJkAZ69/0/*)#20ufqv7z";
        assert_eq!(expected, descriptor.to_string());
        assert_eq!(keymap.len(), 1);

        // try to turn it back into a string with the secrets
        assert_eq!(descriptor_str, descriptor.to_string_with_secret(&keymap));
    }

    #[test]
    fn checksum_for_nested_sh() {
        let descriptor_str = "elsh(wpkh(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL))";
        let descriptor: Descriptor<DescriptorPublicKey> = descriptor_str.parse().unwrap();
        assert_eq!(descriptor.to_string(), "elsh(wpkh(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL))#2040pn7l");

        let descriptor_str = "elsh(wsh(pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL)))";
        let descriptor: Descriptor<DescriptorPublicKey> = descriptor_str.parse().unwrap();
        assert_eq!(descriptor.to_string(), "elsh(wsh(pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL)))#pqs0de7e");
    }

    #[test]
    fn test_xonly_keys() {
        let comp_key = "0308c0fcf8895f4361b4fc77afe2ad53b0bd27dcebfd863421b2b246dc283d4103";
        let x_only_key = "08c0fcf8895f4361b4fc77afe2ad53b0bd27dcebfd863421b2b246dc283d4103";

        // Both x-only keys and comp keys allowed in tr
        Descriptor::<DescriptorPublicKey>::from_str(&format!("eltr({})", comp_key)).unwrap();
        Descriptor::<DescriptorPublicKey>::from_str(&format!("eltr({})", x_only_key)).unwrap();

        // Only compressed keys allowed in wsh
        Descriptor::<DescriptorPublicKey>::from_str(&format!("elwsh(pk({}))", comp_key)).unwrap();
        Descriptor::<DescriptorPublicKey>::from_str(&format!("elwsh(pk({}))", x_only_key))
            .unwrap_err();
    }

    #[test]
    fn test_find_derivation_index_for_spk() {
        let secp = secp256k1_zkp::Secp256k1::verification_only();
        let descriptor = Descriptor::<_, NoExt>::from_str("eltr([73c5da0a/86'/0'/0']xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/*)").unwrap();
        let script_at_0_1 = Script::from_str(
            "5120c73ac1b7a518499b9642aed8cfa15d5401e5bd85ad760b937b69521c297722f0",
        )
        .unwrap();
        let expected_concrete = Descriptor::from_str(
            "eltr(0283dfe85a3151d2517290da461fe2815591ef69f2b18a2ce63f01697a8b313145)",
        )
        .unwrap();

        assert_eq!(
            descriptor.find_derivation_index_for_spk(&secp, &script_at_0_1, 0..1),
            Ok(None)
        );
        assert_eq!(
            descriptor.find_derivation_index_for_spk(&secp, &script_at_0_1, 0..2),
            Ok(Some((1, expected_concrete.clone())))
        );
        assert_eq!(
            descriptor.find_derivation_index_for_spk(&secp, &script_at_0_1, 0..10),
            Ok(Some((1, expected_concrete)))
        );
    }

    #[test]
    fn display_alternate() {
        let bare = StdDescriptor::from_str(
            "elpk(020000000000000000000000000000000000000000000000000000000000000002)",
        )
        .unwrap();
        assert_eq!(
            format!("{}", bare),
            "elpk(020000000000000000000000000000000000000000000000000000000000000002)#vlpqwfjv",
        );
        assert_eq!(
            format!("{:#}", bare),
            "elpk(020000000000000000000000000000000000000000000000000000000000000002)",
        );

        let pkh = StdDescriptor::from_str(
            "elpkh(020000000000000000000000000000000000000000000000000000000000000002)",
        )
        .unwrap();
        assert_eq!(
            format!("{}", pkh),
            "elpkh(020000000000000000000000000000000000000000000000000000000000000002)#jzq8e832",
        );
        assert_eq!(
            format!("{:#}", pkh),
            "elpkh(020000000000000000000000000000000000000000000000000000000000000002)",
        );

        let wpkh = StdDescriptor::from_str(
            "elwpkh(020000000000000000000000000000000000000000000000000000000000000002)",
        )
        .unwrap();
        assert_eq!(
            format!("{}", wpkh),
            "elwpkh(020000000000000000000000000000000000000000000000000000000000000002)#vxhqdpz9",
        );
        assert_eq!(
            format!("{:#}", wpkh),
            "elwpkh(020000000000000000000000000000000000000000000000000000000000000002)",
        );

        let shwpkh = StdDescriptor::from_str(
            "elsh(wpkh(020000000000000000000000000000000000000000000000000000000000000002))",
        )
        .unwrap();
        assert_eq!(
            format!("{}", shwpkh),
            "elsh(wpkh(020000000000000000000000000000000000000000000000000000000000000002))#h9ajn2ft",
        );
        assert_eq!(
            format!("{:#}", shwpkh),
            "elsh(wpkh(020000000000000000000000000000000000000000000000000000000000000002))",
        );

        let wsh = StdDescriptor::from_str("elwsh(1)").unwrap();
        assert_eq!(format!("{}", wsh), "elwsh(1)#s78w5gmj");
        assert_eq!(format!("{:#}", wsh), "elwsh(1)");

        let sh = StdDescriptor::from_str("elsh(1)").unwrap();
        assert_eq!(format!("{}", sh), "elsh(1)#k4aqrx5p");
        assert_eq!(format!("{:#}", sh), "elsh(1)");

        let shwsh = StdDescriptor::from_str("elsh(wsh(1))").unwrap();
        assert_eq!(format!("{}", shwsh), "elsh(wsh(1))#d05z4wjl");
        assert_eq!(format!("{:#}", shwsh), "elsh(wsh(1))");

        let tr = StdDescriptor::from_str(
            "eltr(020000000000000000000000000000000000000000000000000000000000000002)",
        )
        .unwrap();
        assert_eq!(
            format!("{}", tr),
            "eltr(020000000000000000000000000000000000000000000000000000000000000002)#e874qu8z",
        );
        assert_eq!(
            format!("{:#}", tr),
            "eltr(020000000000000000000000000000000000000000000000000000000000000002)",
        );
    }

    #[test]
    fn test_regression_29() {
        let _ = Descriptor::<String>::from_str("eltr(,thresh(1,spk_eq(,00)))");
    }

    #[test]
    fn multipath_descriptors() {
        // We can parse a multipath descriptors, and make it into separate single-path descriptors.
        let desc = Descriptor::<DescriptorPublicKey, NoExt>::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/<7';8h;20>/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/<0;1;987>/*)))").unwrap();
        assert!(desc.is_multipath());
        assert!(!desc.multipath_length_mismatch());
        assert_eq!(desc.into_single_descriptors().unwrap(), vec![
            Descriptor::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/7'/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/0/*)))").unwrap(),
            Descriptor::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/8h/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/1/*)))").unwrap(),
            Descriptor::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/20/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/987/*)))").unwrap()
        ]);

        // Even if only one of the keys is multipath.
        let desc = Descriptor::<DescriptorPublicKey, NoExt>::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/<0;1>/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/*)))").unwrap();
        assert!(desc.is_multipath());
        assert!(!desc.multipath_length_mismatch());
        assert_eq!(desc.into_single_descriptors().unwrap(), vec![
            Descriptor::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/0/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/*)))").unwrap(),
            Descriptor::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/1/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/*)))").unwrap(),
        ]);

        // We can detect regular single-path descriptors.
        let notmulti_desc = Descriptor::<DescriptorPublicKey, NoExt>::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/4567/*)))").unwrap();
        assert!(!notmulti_desc.is_multipath());
        assert!(!notmulti_desc.multipath_length_mismatch());
        assert_eq!(
            notmulti_desc.clone().into_single_descriptors().unwrap(),
            vec![notmulti_desc]
        );

        // We refuse to parse multipath descriptors with a mismatch in the number of derivation paths between keys.
        Descriptor::<DescriptorPublicKey>::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/<0;1>/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/<0;1;2;3;4>/*)))").unwrap_err();
        Descriptor::<DescriptorPublicKey>::from_str("elwsh(andor(pk(tpubDEN9WSToTyy9ZQfaYqSKfmVqmq1VVLNtYfj3Vkqh67et57eJ5sTKZQBkHqSwPUsoSskJeaYnPttHe2VrkCsKA27kUaN9SDc5zhqeLzKa1rr/0'/<0;1;2;3>/*),older(10000),pk(tpubD8LYfn6njiA2inCoxwM7EuN3cuLVcaHAwLYeups13dpevd3nHLRdK9NdQksWXrhLQVxcUZRpnp5CkJ1FhE61WRAsHxDNAkvGkoQkAeWDYjV/8/<0;1;2>/*)))").unwrap_err();
    }
}
