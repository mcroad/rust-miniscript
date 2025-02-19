// Miniscript
// Written in 2019 by
//     Andrew Poelstra <apoelstra@wpsoftware.net>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

//! # Partially-Signed Bitcoin Transactions
//!
//! This module implements the Finalizer and Extractor roles defined in
//! BIP 174, PSBT, described at
//! `https://github.com/bitcoin/bips/blob/master/bip-0174.mediawiki`
//!

use core::fmt;
use core::ops::Deref;
#[cfg(feature = "std")]
use std::error;

use bitcoin::hashes::{hash160, ripemd160, sha256, sha256d};
use bitcoin::secp256k1::{self, Secp256k1};
use bitcoin::util::psbt::{self, PartiallySignedTransaction as Psbt};
use bitcoin::util::sighash::SighashCache;
use bitcoin::util::taproot::{self, ControlBlock, LeafVersion, TapLeafHash};
use bitcoin::{self, EcdsaSighashType, SchnorrSighashType, Script};

use crate::miniscript::iter::PkPkh;
use crate::miniscript::limits::SEQUENCE_LOCKTIME_DISABLE_FLAG;
use crate::miniscript::satisfy::{After, Older};
use crate::prelude::*;
use crate::{
    descriptor, interpreter, Descriptor, DescriptorPublicKey, MiniscriptKey, Preimage32, Satisfier,
    ToPublicKey, TranslatePk, TranslatePk2,
};

mod finalizer;

#[allow(deprecated)]
pub use self::finalizer::{finalize, finalize_mall, interpreter_check};

/// Error type for entire Psbt
#[derive(Debug)]
pub enum Error {
    /// Input Error type
    InputError(InputError, usize),
    /// Wrong Input Count
    WrongInputCount {
        /// Input count in tx
        in_tx: usize,
        /// Input count in psbt
        in_map: usize,
    },
    /// Psbt Input index out of bounds
    InputIdxOutofBounds {
        /// Inputs in pbst
        psbt_inp: usize,
        /// requested index
        index: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::InputError(ref inp_err, index) => write!(f, "{} at index {}", inp_err, index),
            Error::WrongInputCount { in_tx, in_map } => write!(
                f,
                "PSBT had {} inputs in transaction but {} inputs in map",
                in_tx, in_map
            ),
            Error::InputIdxOutofBounds { psbt_inp, index } => write!(
                f,
                "psbt input index {} out of bounds: psbt.inputs.len() {}",
                index, psbt_inp
            ),
        }
    }
}

#[cfg(feature = "std")]
impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        use self::Error::*;

        match self {
            InputError(e, _) => Some(e),
            WrongInputCount { .. } | InputIdxOutofBounds { .. } => None,
        }
    }
}

/// Error type for Pbst Input
#[derive(Debug)]
pub enum InputError {
    /// Get the secp Errors directly
    SecpErr(bitcoin::secp256k1::Error),
    /// Key errors
    KeyErr(bitcoin::util::key::Error),
    /// Could not satisfy taproot descriptor
    /// This error is returned when both script path and key paths could not be
    /// satisfied. We cannot return a detailed error because we try all miniscripts
    /// in script spend path, we cannot know which miniscript failed.
    CouldNotSatisfyTr,
    /// Error doing an interpreter-check on a finalized psbt
    Interpreter(interpreter::Error),
    /// Redeem script does not match the p2sh hash
    InvalidRedeemScript {
        /// Redeem script
        redeem: Script,
        /// Expected p2sh Script
        p2sh_expected: Script,
    },
    /// Witness script does not match the p2wsh hash
    InvalidWitnessScript {
        /// Witness Script
        witness_script: Script,
        /// Expected p2wsh script
        p2wsh_expected: Script,
    },
    /// Invalid sig
    InvalidSignature {
        /// The bitcoin public key
        pubkey: bitcoin::PublicKey,
        /// The (incorrect) signature
        sig: Vec<u8>,
    },
    /// Pass through the underlying errors in miniscript
    MiniscriptError(super::Error),
    /// Missing redeem script for p2sh
    MissingRedeemScript,
    /// Missing witness
    MissingWitness,
    /// used for public key corresponding to pkh/wpkh
    MissingPubkey,
    /// Missing witness script for segwit descriptors
    MissingWitnessScript,
    ///Missing both the witness and non-witness utxo
    MissingUtxo,
    /// Non empty Witness script for p2sh
    NonEmptyWitnessScript,
    /// Non empty Redeem script
    NonEmptyRedeemScript,
    /// Non Standard sighash type
    NonStandardSighashType(bitcoin::blockdata::transaction::NonStandardSighashType),
    /// Sighash did not match
    WrongSighashFlag {
        /// required sighash type
        required: bitcoin::EcdsaSighashType,
        /// the sighash type we got
        got: bitcoin::EcdsaSighashType,
        /// the corresponding publickey
        pubkey: bitcoin::PublicKey,
    },
}

#[cfg(feature = "std")]
impl error::Error for InputError {
    fn cause(&self) -> Option<&dyn error::Error> {
        use self::InputError::*;

        match self {
            CouldNotSatisfyTr
            | InvalidRedeemScript { .. }
            | InvalidWitnessScript { .. }
            | InvalidSignature { .. }
            | MissingRedeemScript
            | MissingWitness
            | MissingPubkey
            | MissingWitnessScript
            | MissingUtxo
            | NonEmptyWitnessScript
            | NonEmptyRedeemScript
            | NonStandardSighashType(_)
            | WrongSighashFlag { .. } => None,
            SecpErr(e) => Some(e),
            KeyErr(e) => Some(e),
            Interpreter(e) => Some(e),
            MiniscriptError(e) => Some(e),
        }
    }
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            InputError::InvalidSignature {
                ref pubkey,
                ref sig,
            } => write!(f, "PSBT: bad signature {} for key {:?}", pubkey, sig),
            InputError::KeyErr(ref e) => write!(f, "Key Err: {}", e),
            InputError::Interpreter(ref e) => write!(f, "Interpreter: {}", e),
            InputError::SecpErr(ref e) => write!(f, "Secp Err: {}", e),
            InputError::InvalidRedeemScript {
                ref redeem,
                ref p2sh_expected,
            } => write!(
                f,
                "Redeem script {} does not match the p2sh script {}",
                redeem, p2sh_expected
            ),
            InputError::InvalidWitnessScript {
                ref witness_script,
                ref p2wsh_expected,
            } => write!(
                f,
                "Witness script {} does not match the p2wsh script {}",
                witness_script, p2wsh_expected
            ),
            InputError::MiniscriptError(ref e) => write!(f, "Miniscript Error: {}", e),
            InputError::MissingWitness => write!(f, "PSBT is missing witness"),
            InputError::MissingRedeemScript => write!(f, "PSBT is Redeem script"),
            InputError::MissingUtxo => {
                write!(f, "PSBT is missing both witness and non-witness UTXO")
            }
            InputError::MissingWitnessScript => write!(f, "PSBT is missing witness script"),
            InputError::MissingPubkey => write!(f, "Missing pubkey for a pkh/wpkh"),
            InputError::NonEmptyRedeemScript => write!(
                f,
                "PSBT has non-empty redeem script at for legacy transactions"
            ),
            InputError::NonEmptyWitnessScript => {
                write!(f, "PSBT has non-empty witness script at for legacy input")
            }
            InputError::WrongSighashFlag {
                required,
                got,
                pubkey,
            } => write!(
                f,
                "PSBT: signature with key {:?} had \
                 sighashflag {:?} rather than required {:?}",
                pubkey, got, required
            ),
            InputError::CouldNotSatisfyTr => {
                write!(f, "Could not satisfy Tr descriptor")
            }
            InputError::NonStandardSighashType(e) => write!(f, "Non-standard sighash type {}", e),
        }
    }
}

#[doc(hidden)]
impl From<super::Error> for InputError {
    fn from(e: super::Error) -> InputError {
        InputError::MiniscriptError(e)
    }
}

#[doc(hidden)]
impl From<bitcoin::secp256k1::Error> for InputError {
    fn from(e: bitcoin::secp256k1::Error) -> InputError {
        InputError::SecpErr(e)
    }
}

#[doc(hidden)]
impl From<bitcoin::util::key::Error> for InputError {
    fn from(e: bitcoin::util::key::Error) -> InputError {
        InputError::KeyErr(e)
    }
}

/// Psbt satisfier for at inputs at a particular index
/// Takes in &psbt because multiple inputs will share
/// the same psbt structure
/// All operations on this structure will panic if index
/// is more than number of inputs in pbst
pub struct PsbtInputSatisfier<'psbt> {
    /// pbst
    pub psbt: &'psbt Psbt,
    /// input index
    pub index: usize,
}

impl<'psbt> PsbtInputSatisfier<'psbt> {
    /// create a new PsbtInputsatisfier from
    /// psbt and index
    pub fn new(psbt: &'psbt Psbt, index: usize) -> Self {
        Self { psbt, index }
    }
}

impl<'psbt, Pk: MiniscriptKey + ToPublicKey> Satisfier<Pk> for PsbtInputSatisfier<'psbt> {
    fn lookup_tap_key_spend_sig(&self) -> Option<bitcoin::SchnorrSig> {
        self.psbt.inputs[self.index].tap_key_sig
    }

    fn lookup_tap_leaf_script_sig(&self, pk: &Pk, lh: &TapLeafHash) -> Option<bitcoin::SchnorrSig> {
        self.psbt.inputs[self.index]
            .tap_script_sigs
            .get(&(pk.to_x_only_pubkey(), *lh))
            .copied()
    }

    fn lookup_tap_control_block_map(
        &self,
    ) -> Option<&BTreeMap<ControlBlock, (bitcoin::Script, LeafVersion)>> {
        Some(&self.psbt.inputs[self.index].tap_scripts)
    }

    fn lookup_pkh_tap_leaf_script_sig(
        &self,
        pkh: &(Pk::Hash, TapLeafHash),
    ) -> Option<(bitcoin::secp256k1::XOnlyPublicKey, bitcoin::SchnorrSig)> {
        self.psbt.inputs[self.index]
            .tap_script_sigs
            .iter()
            .find(|&((pubkey, lh), _sig)| {
                pubkey.to_pubkeyhash() == Pk::hash_to_hash160(&pkh.0) && *lh == pkh.1
            })
            .map(|((x_only_pk, _leaf_hash), sig)| (*x_only_pk, *sig))
    }

    fn lookup_ecdsa_sig(&self, pk: &Pk) -> Option<bitcoin::EcdsaSig> {
        self.psbt.inputs[self.index]
            .partial_sigs
            .get(&pk.to_public_key())
            .copied()
    }

    fn lookup_pkh_ecdsa_sig(
        &self,
        pkh: &Pk::Hash,
    ) -> Option<(bitcoin::PublicKey, bitcoin::EcdsaSig)> {
        self.psbt.inputs[self.index]
            .partial_sigs
            .iter()
            .find(|&(pubkey, _sig)| pubkey.to_pubkeyhash() == Pk::hash_to_hash160(pkh))
            .map(|(pk, sig)| (*pk, *sig))
    }

    fn check_after(&self, n: u32) -> bool {
        let locktime = self.psbt.unsigned_tx.lock_time;
        let seq = self.psbt.unsigned_tx.input[self.index].sequence;

        // https://github.com/bitcoin/bips/blob/master/bip-0065.mediawiki
        // fail if TxIn is finalized
        if seq == 0xffffffff {
            false
        } else {
            <dyn Satisfier<Pk>>::check_after(&After(locktime), n)
        }
    }

    fn check_older(&self, n: u32) -> bool {
        let seq = self.psbt.unsigned_tx.input[self.index].sequence;
        // https://github.com/bitcoin/bips/blob/master/bip-0112.mediawiki
        // Disable flag set. return true
        if n & SEQUENCE_LOCKTIME_DISABLE_FLAG != 0 {
            true
        } else if self.psbt.unsigned_tx.version < 2 || (seq & SEQUENCE_LOCKTIME_DISABLE_FLAG != 0) {
            // transaction version and sequence check
            false
        } else {
            <dyn Satisfier<Pk>>::check_older(&Older(seq), n)
        }
    }

    fn lookup_hash160(&self, h: hash160::Hash) -> Option<Preimage32> {
        self.psbt.inputs[self.index]
            .hash160_preimages
            .get(&h)
            .and_then(try_vec_as_preimage32)
    }

    fn lookup_sha256(&self, h: sha256::Hash) -> Option<Preimage32> {
        self.psbt.inputs[self.index]
            .sha256_preimages
            .get(&h)
            .and_then(try_vec_as_preimage32)
    }

    fn lookup_hash256(&self, h: sha256d::Hash) -> Option<Preimage32> {
        self.psbt.inputs[self.index]
            .hash256_preimages
            .get(&h)
            .and_then(try_vec_as_preimage32)
    }

    fn lookup_ripemd160(&self, h: ripemd160::Hash) -> Option<Preimage32> {
        self.psbt.inputs[self.index]
            .ripemd160_preimages
            .get(&h)
            .and_then(try_vec_as_preimage32)
    }
}

fn try_vec_as_preimage32(vec: &Vec<u8>) -> Option<Preimage32> {
    if vec.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(vec);
        Some(arr)
    } else {
        None
    }
}

// Basic sanity checks on psbts.
// rust-bitcoin TODO: (Long term)
// Brainstorm about how we can enforce these in type system while having a nice API
fn sanity_check(psbt: &Psbt) -> Result<(), Error> {
    if psbt.unsigned_tx.input.len() != psbt.inputs.len() {
        return Err(Error::WrongInputCount {
            in_tx: psbt.unsigned_tx.input.len(),
            in_map: psbt.inputs.len(),
        });
    }

    // Check well-formedness of input data
    for (index, input) in psbt.inputs.iter().enumerate() {
        // TODO: fix this after https://github.com/rust-bitcoin/rust-bitcoin/issues/838
        let target_ecdsa_sighash_ty = match input.sighash_type {
            Some(psbt_hash_ty) => psbt_hash_ty
                .ecdsa_hash_ty()
                .map_err(|e| Error::InputError(InputError::NonStandardSighashType(e), index))?,
            None => EcdsaSighashType::All,
        };
        for (key, ecdsa_sig) in &input.partial_sigs {
            let flag = bitcoin::EcdsaSighashType::from_standard(ecdsa_sig.hash_ty as u32).map_err(
                |_| {
                    Error::InputError(
                        InputError::Interpreter(interpreter::Error::NonStandardSighash(
                            ecdsa_sig.to_vec(),
                        )),
                        index,
                    )
                },
            )?;
            if target_ecdsa_sighash_ty != flag {
                return Err(Error::InputError(
                    InputError::WrongSighashFlag {
                        required: target_ecdsa_sighash_ty,
                        got: flag,
                        pubkey: *key,
                    },
                    index,
                ));
            }
            // Signatures are well-formed in psbt partial sigs
        }
    }

    Ok(())
}

/// Additional operations for miniscript descriptors for various psbt roles.
/// Note that these APIs would generally error when used on scripts that are not
/// miniscripts.
pub trait PsbtExt {
    /// Finalize the psbt. This function takes in a mutable reference to psbt
    /// and populates the final_witness and final_scriptsig
    /// for all miniscript inputs.
    ///
    /// Finalizes all inputs that it can finalize, and returns an error for each input
    /// that it cannot finalize. Also performs a sanity interpreter check on the
    /// finalized psbt which involves checking the signatures/ preimages/timelocks.
    ///
    /// Input finalization also fails if it is not possible to satisfy any of the inputs non-malleably
    /// See [finalizer::finalize_mall] if you want to allow malleable satisfactions
    ///
    /// For finalizing individual inputs, see also [`PsbtExt::finalize_inp`]
    ///
    /// # Errors:
    ///
    /// - A vector of errors, one of each of failed finalized input
    fn finalize_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<(), Vec<Error>>;

    /// Same as [`PsbtExt::finalize_mut`], but does not mutate the input psbt and
    /// returns a new psbt
    ///
    /// # Errors:
    ///
    /// - Returns a mutated psbt with all inputs `finalize_mut` could finalize
    /// - A vector of input errors, one of each of failed finalized input
    fn finalize<C: secp256k1::Verification>(
        self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<Psbt, (Psbt, Vec<Error>)>;

    /// Same as [PsbtExt::finalize_mut], but allows for malleable satisfactions
    fn finalize_mall_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &Secp256k1<C>,
    ) -> Result<(), Vec<Error>>;

    /// Same as [PsbtExt::finalize], but allows for malleable satisfactions
    fn finalize_mall<C: secp256k1::Verification>(
        self,
        secp: &Secp256k1<C>,
    ) -> Result<Psbt, (Psbt, Vec<Error>)>;

    /// Same as [`PsbtExt::finalize_mut`], but only tries to finalize a single input leaving other
    /// inputs as is. Use this when not all of inputs that you are trying to
    /// satisfy are miniscripts
    ///
    /// # Errors:
    ///
    /// - Input error detailing why the finalization failed. The psbt is not mutated when the finalization fails
    fn finalize_inp_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<(), Error>;

    /// Same as [`PsbtExt::finalize_inp_mut`], but does not mutate the psbt and returns a new one
    ///
    /// # Errors:
    ///  Returns a tuple containing
    /// - Original psbt
    /// - Input Error detailing why the input finalization failed
    fn finalize_inp<C: secp256k1::Verification>(
        self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<Psbt, (Psbt, Error)>;

    /// Same as [`PsbtExt::finalize_inp_mut`], but allows for malleable satisfactions
    fn finalize_inp_mall_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<(), Error>;

    /// Same as [`PsbtExt::finalize_inp`], but allows for malleable satisfactions
    fn finalize_inp_mall<C: secp256k1::Verification>(
        self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<Psbt, (Psbt, Error)>;

    /// Psbt extractor as defined in BIP174 that takes in a psbt reference
    /// and outputs a extracted bitcoin::Transaction
    /// Also does the interpreter sanity check
    /// Will error if the final ScriptSig or final Witness are missing
    /// or the interpreter check fails.
    fn extract<C: secp256k1::Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<bitcoin::Transaction, Error>;

    /// Update PSBT input with a descriptor and check consistency of `*_utxo` fields.
    ///
    /// This is the checked version of [`update_with_descriptor_unchecked`]. It checks that the
    /// `witness_utxo` and `non_witness_utxo` are sane and have a `script_pubkey` that matches the
    /// descriptor. In particular, it makes sure segwit descriptors always have `witness_utxo`
    /// present and pre-segwit descriptors always have `non_witness_utxo` present (and the txid
    /// matches). If both `witness_utxo` and `non_witness_utxo` are present then it also checks they
    /// are consistent with each other.
    ///
    /// Hint: because of the *[segwit bug]* some PSBT signers require that `non_witness_utxo` is
    /// present on segwitv0 inputs regardless but this function doesn't enforce this so you will
    /// have to do this check its presence manually (if it is present this *will* check its
    /// validity).
    ///
    /// The `descriptor` **must not have any wildcards** in it
    /// otherwise an error will be returned however it can (and should) have extended keys in it.
    ///
    /// [`update_with_descriptor_unchecked`]: PsbtInputExt::update_with_descriptor_unchecked
    /// [segwit bug]: https://bitcoinhackers.org/@lukedashjr/104287698361196952
    fn update_input_with_descriptor(
        &mut self,
        input_index: usize,
        descriptor: &Descriptor<DescriptorPublicKey>,
    ) -> Result<(), UtxoUpdateError>;

    /// Get the sighash message(data to sign) at input index `idx` based on the sighash
    /// flag specified in the [`Psbt`] sighash field. If the input sighash flag psbt field is `None`
    /// the [`SchnorrSighashType::Default`](bitcoin::util::sighash::SchnorrSighashType::Default) is chosen
    /// for for taproot spends, otherwise [`EcdsaSignatureHashType::All`](bitcoin::EcdsaSighashType::All) is chosen.
    /// If the utxo at `idx` is a taproot output, returns a [`PsbtSighashMsg::TapSighash`] variant.
    /// If the utxo at `idx` is a pre-taproot output, returns a [`PsbtSighashMsg::EcdsaSighash`] variant.
    /// The `tapleaf_hash` parameter can be used to specify which tapleaf script hash has to be computed. If
    /// `tapleaf_hash` is [`None`], and the output is taproot output, the key spend hash is computed. This parameter must be
    /// set to [`None`] while computing sighash for pre-taproot outputs.
    /// The function also updates the sighash cache with transaction computed during sighash computation of this input
    ///
    /// # Arguments:
    ///
    /// * `idx`: The input index of psbt to sign
    /// * `cache`: The [`SighashCache`] for used to cache/read previously cached computations
    /// * `tapleaf_hash`: If the output is taproot, compute the sighash for this particular leaf.
    ///
    /// [`SighashCache`]: bitcoin::util::sighash::SighashCache
    fn sighash_msg<T: Deref<Target = bitcoin::Transaction>>(
        &self,
        idx: usize,
        cache: &mut SighashCache<T>,
        tapleaf_hash: Option<TapLeafHash>,
    ) -> Result<PsbtSighashMsg, SighashError>;
}

impl PsbtExt for Psbt {
    fn finalize_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<(), Vec<Error>> {
        // Actually construct the witnesses
        let mut errors = vec![];
        for index in 0..self.inputs.len() {
            match finalizer::finalize_input(self, index, secp, /*allow_mall*/ false) {
                Ok(..) => {}
                Err(e) => {
                    errors.push(e);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn finalize<C: secp256k1::Verification>(
        mut self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<Psbt, (Psbt, Vec<Error>)> {
        match self.finalize_mut(secp) {
            Ok(..) => Ok(self),
            Err(e) => Err((self, e)),
        }
    }

    fn finalize_mall_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
    ) -> Result<(), Vec<Error>> {
        let mut errors = vec![];
        for index in 0..self.inputs.len() {
            match finalizer::finalize_input(self, index, secp, /*allow_mall*/ true) {
                Ok(..) => {}
                Err(e) => {
                    errors.push(e);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn finalize_mall<C: secp256k1::Verification>(
        mut self,
        secp: &Secp256k1<C>,
    ) -> Result<Psbt, (Psbt, Vec<Error>)> {
        match self.finalize_mall_mut(secp) {
            Ok(..) => Ok(self),
            Err(e) => Err((self, e)),
        }
    }

    fn finalize_inp_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<(), Error> {
        if index >= self.inputs.len() {
            return Err(Error::InputIdxOutofBounds {
                psbt_inp: self.inputs.len(),
                index,
            });
        }
        finalizer::finalize_input(self, index, secp, /*allow_mall*/ false)
    }

    fn finalize_inp<C: secp256k1::Verification>(
        mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<Psbt, (Psbt, Error)> {
        match self.finalize_inp_mut(secp, index) {
            Ok(..) => Ok(self),
            Err(e) => Err((self, e)),
        }
    }

    fn finalize_inp_mall_mut<C: secp256k1::Verification>(
        &mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<(), Error> {
        if index >= self.inputs.len() {
            return Err(Error::InputIdxOutofBounds {
                psbt_inp: self.inputs.len(),
                index,
            });
        }
        finalizer::finalize_input(self, index, secp, /*allow_mall*/ false)
    }

    fn finalize_inp_mall<C: secp256k1::Verification>(
        mut self,
        secp: &secp256k1::Secp256k1<C>,
        index: usize,
    ) -> Result<Psbt, (Psbt, Error)> {
        match self.finalize_inp_mall_mut(secp, index) {
            Ok(..) => Ok(self),
            Err(e) => Err((self, e)),
        }
    }

    fn extract<C: secp256k1::Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<bitcoin::Transaction, Error> {
        sanity_check(self)?;

        let mut ret = self.unsigned_tx.clone();
        for (n, input) in self.inputs.iter().enumerate() {
            if input.final_script_sig.is_none() && input.final_script_witness.is_none() {
                return Err(Error::InputError(InputError::MissingWitness, n));
            }

            if let Some(witness) = input.final_script_witness.as_ref() {
                ret.input[n].witness = witness.clone();
            }
            if let Some(script_sig) = input.final_script_sig.as_ref() {
                ret.input[n].script_sig = script_sig.clone();
            }
        }
        interpreter_check(self, secp)?;
        Ok(ret)
    }

    fn update_input_with_descriptor(
        &mut self,
        input_index: usize,
        desc: &Descriptor<DescriptorPublicKey>,
    ) -> Result<(), UtxoUpdateError> {
        let n_inputs = self.inputs.len();
        let input = self
            .inputs
            .get_mut(input_index)
            .ok_or(UtxoUpdateError::IndexOutOfBounds(input_index, n_inputs))?;
        let txin = self
            .unsigned_tx
            .input
            .get(input_index)
            .ok_or(UtxoUpdateError::MissingInputUtxo)?;

        let desc_type = desc.desc_type();

        if let Some(non_witness_utxo) = &input.non_witness_utxo {
            if txin.previous_output.txid != non_witness_utxo.txid() {
                return Err(UtxoUpdateError::UtxoCheck);
            }
        }

        let expected_spk = {
            match (&input.witness_utxo, &input.non_witness_utxo) {
                (Some(witness_utxo), None) => {
                    if desc_type.segwit_version().is_some() {
                        witness_utxo.script_pubkey.clone()
                    } else {
                        return Err(UtxoUpdateError::UtxoCheck);
                    }
                }
                (None, Some(non_witness_utxo)) => {
                    if desc_type.segwit_version().is_some() {
                        return Err(UtxoUpdateError::UtxoCheck);
                    }

                    non_witness_utxo
                        .output
                        .get(txin.previous_output.vout as usize)
                        .ok_or(UtxoUpdateError::UtxoCheck)?
                        .script_pubkey
                        .clone()
                }
                (Some(witness_utxo), Some(non_witness_utxo)) => {
                    if witness_utxo
                        != non_witness_utxo
                            .output
                            .get(txin.previous_output.vout as usize)
                            .ok_or(UtxoUpdateError::UtxoCheck)?
                    {
                        return Err(UtxoUpdateError::UtxoCheck);
                    }

                    witness_utxo.script_pubkey.clone()
                }
                (None, None) => return Err(UtxoUpdateError::UtxoCheck),
            }
        };

        let (_, spk_check_passed) =
            update_input_with_descriptor_helper(input, desc, Some(expected_spk))
                .map_err(UtxoUpdateError::DerivationError)?;

        if !spk_check_passed {
            return Err(UtxoUpdateError::MismatchedScriptPubkey);
        }

        Ok(())
    }

    fn sighash_msg<T: Deref<Target = bitcoin::Transaction>>(
        &self,
        idx: usize,
        cache: &mut SighashCache<T>,
        tapleaf_hash: Option<TapLeafHash>,
    ) -> Result<PsbtSighashMsg, SighashError> {
        // Infer a descriptor at idx
        if idx >= self.inputs.len() {
            return Err(SighashError::IndexOutOfBounds(idx, self.inputs.len()));
        }
        let inp = &self.inputs[idx];
        let prevouts = finalizer::prevouts(self).map_err(|_e| SighashError::MissingSpendUtxos)?;
        // Note that as per Psbt spec we should have access to spent_utxos for the transaction
        // Even if the transaction does not require SighashAll, we create `Prevouts::All` for code simplicity
        let prevouts = bitcoin::util::sighash::Prevouts::All(&prevouts);
        let inp_spk =
            finalizer::get_scriptpubkey(self, idx).map_err(|_e| SighashError::MissingInputUtxo)?;
        if inp_spk.is_v1_p2tr() {
            let hash_ty = inp
                .sighash_type
                .map(|sighash_type| sighash_type.schnorr_hash_ty())
                .unwrap_or(Ok(SchnorrSighashType::Default))
                .map_err(|_e| SighashError::InvalidSighashType)?;
            match tapleaf_hash {
                Some(leaf_hash) => {
                    let tap_sighash_msg = cache
                        .taproot_script_spend_signature_hash(idx, &prevouts, leaf_hash, hash_ty)?;
                    Ok(PsbtSighashMsg::TapSighash(tap_sighash_msg))
                }
                None => {
                    let tap_sighash_msg =
                        cache.taproot_key_spend_signature_hash(idx, &prevouts, hash_ty)?;
                    Ok(PsbtSighashMsg::TapSighash(tap_sighash_msg))
                }
            }
        } else {
            let hash_ty = inp
                .sighash_type
                .map(|sighash_type| sighash_type.ecdsa_hash_ty())
                .unwrap_or(Ok(EcdsaSighashType::All))
                .map_err(|_e| SighashError::InvalidSighashType)?;
            let amt = finalizer::get_utxo(self, idx)
                .map_err(|_e| SighashError::MissingInputUtxo)?
                .value;
            let is_nested_wpkh = inp_spk.is_p2sh()
                && inp
                    .redeem_script
                    .as_ref()
                    .map(|x| x.is_v0_p2wpkh())
                    .unwrap_or(false);
            let is_nested_wsh = inp_spk.is_p2sh()
                && inp
                    .redeem_script
                    .as_ref()
                    .map(|x| x.is_v0_p2wsh())
                    .unwrap_or(false);
            if inp_spk.is_v0_p2wpkh() || inp_spk.is_v0_p2wsh() || is_nested_wpkh || is_nested_wsh {
                let msg = if inp_spk.is_v0_p2wpkh() {
                    let script_code = script_code_wpkh(inp_spk);
                    cache.segwit_signature_hash(idx, &script_code, amt, hash_ty)?
                } else if is_nested_wpkh {
                    let script_code = script_code_wpkh(
                        inp.redeem_script
                            .as_ref()
                            .expect("Redeem script non-empty checked earlier"),
                    );
                    cache.segwit_signature_hash(idx, &script_code, amt, hash_ty)?
                } else {
                    // wsh and nested wsh, script code is witness script
                    let script_code = inp
                        .witness_script
                        .as_ref()
                        .ok_or(SighashError::MissingWitnessScript)?;
                    cache.segwit_signature_hash(idx, script_code, amt, hash_ty)?
                };
                Ok(PsbtSighashMsg::EcdsaSighash(msg))
            } else {
                // legacy sighash case
                let script_code = if inp_spk.is_p2sh() {
                    inp.redeem_script
                        .as_ref()
                        .ok_or(SighashError::MissingRedeemScript)?
                } else {
                    inp_spk
                };
                let msg = cache.legacy_signature_hash(idx, script_code, hash_ty.to_u32())?;
                Ok(PsbtSighashMsg::EcdsaSighash(msg))
            }
        }
    }
}

/// Extension trait for PSBT inputs
pub trait PsbtInputExt {
    /// Given the descriptor for a utxo being spent populate the PSBT input's fields so it can be signed.
    ///
    /// If the descriptor contains wildcards or otherwise cannot be transformed into a concrete
    /// descriptor an error will be returned. The descriptor *can* (and should) have extended keys in
    /// it so PSBT fields like `bip32_derivation` and `tap_key_origins` can be populated.
    ///
    /// Note that his method doesn't check that the `witness_utxo` or `non_witness_utxo` is
    /// consistent with the descriptor. To do that see [`update_input_with_descriptor`].
    ///
    /// ## Return value
    ///
    /// For convenience, this returns the concrete descriptor that is computed internally to fill
    /// out the PSBT input fields. This can be used to manually check that the `script_pubkey` in
    /// `witness_utxo` and/or `non_witness_utxo` is consistent with the descriptor.
    ///
    /// [`update_input_with_descriptor`]: PsbtExt::update_input_with_descriptor
    fn update_with_descriptor_unchecked(
        &mut self,
        descriptor: &Descriptor<DescriptorPublicKey>,
    ) -> Result<Descriptor<bitcoin::PublicKey>, descriptor::ConversionError>;
}

impl PsbtInputExt for psbt::Input {
    fn update_with_descriptor_unchecked(
        &mut self,
        descriptor: &Descriptor<DescriptorPublicKey>,
    ) -> Result<Descriptor<bitcoin::PublicKey>, descriptor::ConversionError> {
        let (derived, _) = update_input_with_descriptor_helper(self, descriptor, None)?;
        Ok(derived)
    }
}

fn update_input_with_descriptor_helper(
    input: &mut psbt::Input,
    descriptor: &Descriptor<DescriptorPublicKey>,
    check_script: Option<Script>,
    // the return value is a tuple here since the two internal calls to it require different info.
    // One needs the derived descriptor and the other needs to know whether the script_pubkey check
    // failed.
) -> Result<(Descriptor<bitcoin::PublicKey>, bool), descriptor::ConversionError> {
    use core::cell::RefCell;
    let secp = secp256k1::Secp256k1::verification_only();

    let derived = if let Descriptor::Tr(_) = &descriptor {
        let mut hash_lookup = BTreeMap::new();
        let derived = descriptor.translate_pk(
            |xpk| xpk.derive_public_key(&secp),
            |xpk| {
                let xonly = xpk.derive_public_key(&secp)?.to_x_only_pubkey();
                let hash = xonly.to_pubkeyhash();
                hash_lookup.insert(hash, xonly);
                Ok(hash)
            },
        )?;

        if let Some(check_script) = check_script {
            if check_script != derived.script_pubkey() {
                return Ok((derived, false));
            }
        }

        // NOTE: they will both always be Tr
        if let (Descriptor::Tr(tr_derived), Descriptor::Tr(tr_xpk)) = (&derived, descriptor) {
            let spend_info = tr_derived.spend_info();
            let ik_derived = spend_info.internal_key();
            let ik_xpk = tr_xpk.internal_key();
            input.tap_internal_key = Some(ik_derived);
            input.tap_merkle_root = spend_info.merkle_root();
            input.tap_key_origins.insert(
                ik_derived,
                (
                    vec![],
                    (ik_xpk.master_fingerprint(), ik_xpk.full_derivation_path()),
                ),
            );

            for ((_depth_der, ms_derived), (_depth, ms)) in
                tr_derived.iter_scripts().zip(tr_xpk.iter_scripts())
            {
                debug_assert_eq!(_depth_der, _depth);
                let leaf_script = (ms_derived.encode(), LeafVersion::TapScript);
                let tapleaf_hash = TapLeafHash::from_script(&leaf_script.0, leaf_script.1);
                let control_block = spend_info
                    .control_block(&leaf_script)
                    .expect("Control block must exist in script map for every known leaf");
                input.tap_scripts.insert(control_block, leaf_script);

                for (pk_pkh_derived, pk_pkh_xpk) in ms_derived.iter_pk_pkh().zip(ms.iter_pk_pkh()) {
                    let (xonly, xpk) = match (pk_pkh_derived, pk_pkh_xpk) {
                        (PkPkh::PlainPubkey(pk), PkPkh::PlainPubkey(xpk)) => {
                            (pk.to_x_only_pubkey(), xpk)
                        }
                        (PkPkh::HashedPubkey(hash), PkPkh::HashedPubkey(xpk)) => (
                            *hash_lookup
                                .get(&hash)
                                .expect("translate_pk inserted an entry for every hash"),
                            xpk,
                        ),
                        _ => unreachable!("the iterators work in the same order"),
                    };

                    input
                        .tap_key_origins
                        .entry(xonly)
                        .and_modify(|(tapleaf_hashes, _)| {
                            if tapleaf_hashes.last() != Some(&tapleaf_hash) {
                                tapleaf_hashes.push(tapleaf_hash);
                            }
                        })
                        .or_insert_with(|| {
                            (
                                vec![tapleaf_hash],
                                (xpk.master_fingerprint(), xpk.full_derivation_path()),
                            )
                        });
                }
            }
        }

        derived
    } else {
        // have to use a RefCell because we can't pass FnMut to translate_pk2
        let bip32_derivation = RefCell::new(BTreeMap::new());
        let derived = descriptor.translate_pk2(|xpk| {
            let derived = xpk.derive_public_key(&secp)?;
            bip32_derivation.borrow_mut().insert(
                derived.to_public_key().inner,
                (xpk.master_fingerprint(), xpk.full_derivation_path()),
            );
            Ok(derived)
        })?;

        if let Some(check_script) = check_script {
            if check_script != derived.script_pubkey() {
                return Ok((derived, false));
            }
        }

        input.bip32_derivation = bip32_derivation.into_inner();

        match &derived {
            Descriptor::Bare(_) | Descriptor::Pkh(_) | Descriptor::Wpkh(_) => {}
            Descriptor::Sh(sh) => match sh.as_inner() {
                descriptor::ShInner::Wsh(wsh) => {
                    input.witness_script = Some(wsh.inner_script());
                    input.redeem_script = Some(wsh.inner_script().to_v0_p2wsh());
                }
                descriptor::ShInner::Wpkh(..) => input.redeem_script = Some(sh.inner_script()),
                descriptor::ShInner::SortedMulti(_) | descriptor::ShInner::Ms(_) => {
                    input.redeem_script = Some(sh.inner_script())
                }
            },
            Descriptor::Wsh(wsh) => input.witness_script = Some(wsh.inner_script()),
            Descriptor::Tr(_) => unreachable!("Tr is dealt with separately"),
        }

        derived
    };

    Ok((derived, true))
}

// Get a script from witness script pubkey hash
fn script_code_wpkh(script: &Script) -> Script {
    assert!(script.is_v0_p2wpkh());
    // ugly segwit stuff
    let mut script_code = vec![0x76u8, 0xa9, 0x14];
    script_code.extend(&script.as_bytes()[2..]);
    script_code.push(0x88);
    script_code.push(0xac);
    Script::from(script_code)
}

/// Return error type for [`PsbtExt::update_input_with_descriptor`]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum UtxoUpdateError {
    /// Index out of bounds
    IndexOutOfBounds(usize, usize),
    /// The PSBT transaction didn't have an input at that index
    MissingInputUtxo,
    /// Derivation error
    DerivationError(descriptor::ConversionError),
    /// The PSBT's `witness_utxo` and/or `non_witness_utxo` were invalid or missing
    UtxoCheck,
    /// The PSBT's `witness_utxo` and/or `non_witness_utxo` had a script_pubkey that did not match
    /// the descriptor
    MismatchedScriptPubkey,
}

impl fmt::Display for UtxoUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UtxoUpdateError::IndexOutOfBounds(ind, len) => {
                write!(f, "index {}, psbt input len: {}", ind, len)
            }
            UtxoUpdateError::MissingInputUtxo => write!(f, "Missing input utxo in pbst"),
            UtxoUpdateError::DerivationError(e) => write!(f, "Key derivation error {}", e),
            UtxoUpdateError::UtxoCheck => write!(
                f,
                "The input's witness_utxo and/or non_witness_utxo were invalid or missing"
            ),
            UtxoUpdateError::MismatchedScriptPubkey => {
                write!(f, "The input's witness_utxo and/or non_witness_utxo had a script pubkey that didn't match the descriptor")
            }
        }
    }
}

#[cfg(feature = "std")]
impl error::Error for UtxoUpdateError {
    fn cause(&self) -> Option<&dyn error::Error> {
        use self::UtxoUpdateError::*;

        match self {
            IndexOutOfBounds(_, _) | MissingInputUtxo | UtxoCheck | MismatchedScriptPubkey => None,
            DerivationError(e) => Some(e),
        }
    }
}

/// Return error type for [`PsbtExt::sighash_msg`]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum SighashError {
    /// Index out of bounds
    IndexOutOfBounds(usize, usize),
    /// Missing input utxo
    MissingInputUtxo,
    /// Missing Prevouts
    MissingSpendUtxos,
    /// Invalid Sighash type
    InvalidSighashType,
    /// Sighash computation error
    /// Only happens when single does not have corresponding output as psbts
    /// already have information to compute the sighash
    SighashComputationError(bitcoin::util::sighash::Error),
    /// Missing Witness script
    MissingWitnessScript,
    /// Missing Redeem script,
    MissingRedeemScript,
}

impl fmt::Display for SighashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SighashError::IndexOutOfBounds(ind, len) => {
                write!(f, "index {}, psbt input len: {}", ind, len)
            }
            SighashError::MissingInputUtxo => write!(f, "Missing input utxo in pbst"),
            SighashError::MissingSpendUtxos => write!(f, "Missing Psbt spend utxos"),
            SighashError::InvalidSighashType => write!(f, "Invalid Sighash type"),
            SighashError::SighashComputationError(e) => {
                write!(f, "Sighash computation error : {}", e)
            }
            SighashError::MissingWitnessScript => write!(f, "Missing Witness Script"),
            SighashError::MissingRedeemScript => write!(f, "Missing Redeem Script"),
        }
    }
}

#[cfg(feature = "std")]
impl error::Error for SighashError {
    fn cause(&self) -> Option<&dyn error::Error> {
        use self::SighashError::*;

        match self {
            IndexOutOfBounds(_, _)
            | MissingInputUtxo
            | MissingSpendUtxos
            | InvalidSighashType
            | MissingWitnessScript
            | MissingRedeemScript => None,
            SighashComputationError(e) => Some(e),
        }
    }
}

impl From<bitcoin::util::sighash::Error> for SighashError {
    fn from(e: bitcoin::util::sighash::Error) -> Self {
        SighashError::SighashComputationError(e)
    }
}

/// Sighash message(signing data) for a given psbt transaction input.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum PsbtSighashMsg {
    /// Taproot Signature hash
    TapSighash(taproot::TapSighashHash),
    /// Ecdsa Sighash message (includes sighash for legacy/p2sh/segwitv0 outputs)
    EcdsaSighash(bitcoin::Sighash),
}

impl PsbtSighashMsg {
    /// Convert the message to a [`secp256k1::Message`].
    pub fn to_secp_msg(&self) -> secp256k1::Message {
        match *self {
            PsbtSighashMsg::TapSighash(msg) => {
                secp256k1::Message::from_slice(msg.as_ref()).expect("Sighashes are 32 bytes")
            }
            PsbtSighashMsg::EcdsaSighash(msg) => {
                secp256k1::Message::from_slice(msg.as_ref()).expect("Sighashes are 32 bytes")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::consensus::encode::deserialize;
    use bitcoin::hashes::hex::FromHex;
    use bitcoin::secp256k1::PublicKey;
    use bitcoin::util::bip32::{DerivationPath, ExtendedPubKey};
    use bitcoin::{OutPoint, TxIn, TxOut, XOnlyPublicKey};

    use super::*;
    use crate::Miniscript;

    #[test]
    fn test_extract_bip174() {
        let psbt: bitcoin::util::psbt::PartiallySignedTransaction = deserialize(&Vec::<u8>::from_hex("70736274ff01009a020000000258e87a21b56daf0c23be8e7070456c336f7cbaa5c8757924f545887bb2abdd750000000000ffffffff838d0427d0ec650a68aa46bb0b098aea4422c071b2ca78352a077959d07cea1d0100000000ffffffff0270aaf00800000000160014d85c2b71d0060b09c9886aeb815e50991dda124d00e1f5050000000016001400aea9a2e5f0f876a588df5546e8742d1d87008f00000000000100bb0200000001aad73931018bd25f84ae400b68848be09db706eac2ac18298babee71ab656f8b0000000048473044022058f6fc7c6a33e1b31548d481c826c015bd30135aad42cd67790dab66d2ad243b02204a1ced2604c6735b6393e5b41691dd78b00f0c5942fb9f751856faa938157dba01feffffff0280f0fa020000000017a9140fb9463421696b82c833af241c78c17ddbde493487d0f20a270100000017a91429ca74f8a08f81999428185c97b5d852e4063f6187650000000107da00473044022074018ad4180097b873323c0015720b3684cc8123891048e7dbcd9b55ad679c99022073d369b740e3eb53dcefa33823c8070514ca55a7dd9544f157c167913261118c01483045022100f61038b308dc1da865a34852746f015772934208c6d24454393cd99bdf2217770220056e675a675a6d0a02b85b14e5e29074d8a25a9b5760bea2816f661910a006ea01475221029583bf39ae0a609747ad199addd634fa6108559d6c5cd39b4c2183f1ab96e07f2102dab61ff49a14db6a7d02b0cd1fbb78fc4b18312b5b4e54dae4dba2fbfef536d752ae0001012000c2eb0b0000000017a914b7f5faf40e3d40a5a459b1db3535f2b72fa921e8870107232200208c2353173743b595dfb4a07b72ba8e42e3797da74e87fe7d9d7497e3b20289030108da0400473044022062eb7a556107a7c73f45ac4ab5a1dddf6f7075fb1275969a7f383efff784bcb202200c05dbb7470dbf2f08557dd356c7325c1ed30913e996cd3840945db12228da5f01473044022065f45ba5998b59a27ffe1a7bed016af1f1f90d54b3aa8f7450aa5f56a25103bd02207f724703ad1edb96680b284b56d4ffcb88f7fb759eabbe08aa30f29b851383d20147522103089dc10c7ac6db54f91329af617333db388cead0c231f723379d1b99030b02dc21023add904f3d6dcf59ddb906b0dee23529b7ffb9ed50e5e86151926860221f0e7352ae00220203a9a4c37f5996d3aa25dbac6b570af0650394492942460b354753ed9eeca5877110d90c6a4f000000800000008004000080002202027f6399757d2eff55a136ad02c684b1838b6556e5f1b6b34282a94b6b5005109610d90c6a4f00000080000000800500008000").unwrap()).unwrap();
        let secp = Secp256k1::verification_only();
        let tx = psbt.extract(&secp).unwrap();
        let expected: bitcoin::Transaction = deserialize(&Vec::<u8>::from_hex("0200000000010258e87a21b56daf0c23be8e7070456c336f7cbaa5c8757924f545887bb2abdd7500000000da00473044022074018ad4180097b873323c0015720b3684cc8123891048e7dbcd9b55ad679c99022073d369b740e3eb53dcefa33823c8070514ca55a7dd9544f157c167913261118c01483045022100f61038b308dc1da865a34852746f015772934208c6d24454393cd99bdf2217770220056e675a675a6d0a02b85b14e5e29074d8a25a9b5760bea2816f661910a006ea01475221029583bf39ae0a609747ad199addd634fa6108559d6c5cd39b4c2183f1ab96e07f2102dab61ff49a14db6a7d02b0cd1fbb78fc4b18312b5b4e54dae4dba2fbfef536d752aeffffffff838d0427d0ec650a68aa46bb0b098aea4422c071b2ca78352a077959d07cea1d01000000232200208c2353173743b595dfb4a07b72ba8e42e3797da74e87fe7d9d7497e3b2028903ffffffff0270aaf00800000000160014d85c2b71d0060b09c9886aeb815e50991dda124d00e1f5050000000016001400aea9a2e5f0f876a588df5546e8742d1d87008f000400473044022062eb7a556107a7c73f45ac4ab5a1dddf6f7075fb1275969a7f383efff784bcb202200c05dbb7470dbf2f08557dd356c7325c1ed30913e996cd3840945db12228da5f01473044022065f45ba5998b59a27ffe1a7bed016af1f1f90d54b3aa8f7450aa5f56a25103bd02207f724703ad1edb96680b284b56d4ffcb88f7fb759eabbe08aa30f29b851383d20147522103089dc10c7ac6db54f91329af617333db388cead0c231f723379d1b99030b02dc21023add904f3d6dcf59ddb906b0dee23529b7ffb9ed50e5e86151926860221f0e7352ae00000000").unwrap()).unwrap();
        assert_eq!(tx, expected);
    }

    #[test]
    fn test_update_input_tr_no_script() {
        // keys taken from: https://github.com/bitcoin/bips/blob/master/bip-0086.mediawiki#Specifications
        let root_xpub = ExtendedPubKey::from_str("xpub661MyMwAqRbcFkPHucMnrGNzDwb6teAX1RbKQmqtEF8kK3Z7LZ59qafCjB9eCRLiTVG3uxBxgKvRgbubRhqSKXnGGb1aoaqLrpMBDrVxga8").unwrap();
        let fingerprint = root_xpub.fingerprint();
        let desc = format!("tr([{}/86'/0'/0']xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/0)", fingerprint);
        let desc = Descriptor::from_str(&desc).unwrap();
        let mut psbt_input = psbt::Input::default();
        psbt_input.update_with_descriptor_unchecked(&desc).unwrap();
        let internal_key = XOnlyPublicKey::from_str(
            "cc8a4bc64d897bddc5fbc2f670f7a8ba0b386779106cf1223c6fc5d7cd6fc115",
        )
        .unwrap();
        assert_eq!(psbt_input.tap_internal_key, Some(internal_key));
        assert_eq!(
            psbt_input.tap_key_origins.get(&internal_key),
            Some(&(
                vec![],
                (
                    fingerprint,
                    DerivationPath::from_str("m/86'/0'/0'/0/0").unwrap()
                )
            ))
        );
        assert_eq!(psbt_input.tap_key_origins.len(), 1);
        assert_eq!(psbt_input.tap_scripts.len(), 0);
        assert_eq!(psbt_input.tap_merkle_root, None);
    }

    #[test]
    fn test_update_input_tr_with_tapscript() {
        use crate::Tap;
        // keys taken from: https://github.com/bitcoin/bips/blob/master/bip-0086.mediawiki#Specifications
        let root_xpub = ExtendedPubKey::from_str("xpub661MyMwAqRbcFkPHucMnrGNzDwb6teAX1RbKQmqtEF8kK3Z7LZ59qafCjB9eCRLiTVG3uxBxgKvRgbubRhqSKXnGGb1aoaqLrpMBDrVxga8").unwrap();
        let fingerprint = root_xpub.fingerprint();
        let xpub = format!("[{}/86'/0'/0']xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ", fingerprint);
        let desc = format!(
            "tr({}/0/0,{{pkh({}/0/1),multi_a(2,{}/0/1,{}/1/0)}})",
            xpub, xpub, xpub, xpub
        );

        let desc = Descriptor::from_str(&desc).unwrap();
        let internal_key = XOnlyPublicKey::from_str(
            "cc8a4bc64d897bddc5fbc2f670f7a8ba0b386779106cf1223c6fc5d7cd6fc115",
        )
        .unwrap();
        let mut psbt_input = psbt::Input::default();
        psbt_input.update_with_descriptor_unchecked(&desc).unwrap();
        assert_eq!(psbt_input.tap_internal_key, Some(internal_key));
        assert_eq!(
            psbt_input.tap_key_origins.get(&internal_key),
            Some(&(
                vec![],
                (
                    fingerprint,
                    DerivationPath::from_str("m/86'/0'/0'/0/0").unwrap()
                )
            ))
        );
        assert_eq!(psbt_input.tap_key_origins.len(), 3);
        assert_eq!(psbt_input.tap_scripts.len(), 2);
        assert!(psbt_input.tap_merkle_root.is_some());

        let key_0_1 = XOnlyPublicKey::from_str(
            "83dfe85a3151d2517290da461fe2815591ef69f2b18a2ce63f01697a8b313145",
        )
        .unwrap();
        let first_leaf_hash = {
            let ms = Miniscript::<XOnlyPublicKey, Tap>::from_str(&format!(
                "pkh({})",
                &key_0_1.to_pubkeyhash()
            ))
            .unwrap();
            let first_script = ms.encode();
            assert!(psbt_input
                .tap_scripts
                .values()
                .find(|value| *value == &(first_script.clone(), LeafVersion::TapScript))
                .is_some());
            TapLeafHash::from_script(&first_script, LeafVersion::TapScript)
        };

        {
            // check 0/1
            let (leaf_hashes, (key_fingerprint, deriv_path)) =
                psbt_input.tap_key_origins.get(&key_0_1).unwrap();
            assert_eq!(key_fingerprint, &fingerprint);
            assert_eq!(&deriv_path.to_string(), "m/86'/0'/0'/0/1");
            assert_eq!(leaf_hashes.len(), 2);
            assert!(leaf_hashes.contains(&first_leaf_hash));
        }

        {
            // check 1/0
            let key_1_0 = XOnlyPublicKey::from_str(
                "399f1b2f4393f29a18c937859c5dd8a77350103157eb880f02e8c08214277cef",
            )
            .unwrap();
            let (leaf_hashes, (key_fingerprint, deriv_path)) =
                psbt_input.tap_key_origins.get(&key_1_0).unwrap();
            assert_eq!(key_fingerprint, &fingerprint);
            assert_eq!(&deriv_path.to_string(), "m/86'/0'/0'/1/0");
            assert_eq!(leaf_hashes.len(), 1);
            assert!(!leaf_hashes.contains(&first_leaf_hash));
        }
    }

    #[test]
    fn test_update_input_non_tr_multi() {
        // values taken from https://github.com/bitcoin/bips/blob/master/bip-0084.mediawiki (after removing zpub thingy)
        let root_xpub = ExtendedPubKey::from_str("xpub661MyMwAqRbcFkPHucMnrGNzDwb6teAX1RbKQmqtEF8kK3Z7LZ59qafCjB9eCRLiTVG3uxBxgKvRgbubRhqSKXnGGb1aoaqLrpMBDrVxga8").unwrap();
        let fingerprint = root_xpub.fingerprint();
        let xpub = format!("[{}/84'/0'/0']xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V", fingerprint);
        let pubkeys = [
            "0330d54fd0dd420a6e5f8d3624f5f3482cae350f79d5f0753bf5beef9c2d91af3c",
            "03e775fd51f0dfb8cd865d9ff1cca2a158cf651fe997fdc9fee9c1d3b5e995ea77",
            "03025324888e429ab8e3dbaf1f7802648b9cd01e9b418485c5fa4c1b9b5700e1a6",
        ];

        let expected_bip32 = pubkeys
            .iter()
            .zip(["0/0", "0/1", "1/0"].iter())
            .map(|(pubkey, path)| {
                (
                    PublicKey::from_str(pubkey).unwrap(),
                    (
                        fingerprint,
                        DerivationPath::from_str(&format!("m/84'/0'/0'/{}", path)).unwrap(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        {
            // test segwit
            let desc = format!("wsh(multi(2,{}/0/0,{}/0/1,{}/1/0))", xpub, xpub, xpub);
            let desc = Descriptor::from_str(&desc).unwrap();
            let derived = format!("wsh(multi(2,{}))", pubkeys.join(","));
            let derived = Descriptor::<bitcoin::PublicKey>::from_str(&derived).unwrap();

            let mut psbt_input = psbt::Input::default();
            psbt_input.update_with_descriptor_unchecked(&desc).unwrap();

            assert_eq!(expected_bip32, psbt_input.bip32_derivation);
            assert_eq!(
                psbt_input.witness_script,
                Some(derived.explicit_script().unwrap())
            );
        }

        {
            // test non-segwit
            let desc = format!("sh(multi(2,{}/0/0,{}/0/1,{}/1/0))", xpub, xpub, xpub);
            let desc = Descriptor::from_str(&desc).unwrap();
            let derived = format!("sh(multi(2,{}))", pubkeys.join(","));
            let derived = Descriptor::<bitcoin::PublicKey>::from_str(&derived).unwrap();
            let mut psbt_input = psbt::Input::default();

            psbt_input.update_with_descriptor_unchecked(&desc).unwrap();

            assert_eq!(psbt_input.bip32_derivation, expected_bip32);
            assert_eq!(psbt_input.witness_script, None);
            assert_eq!(
                psbt_input.redeem_script,
                Some(derived.explicit_script().unwrap())
            );
        }
    }

    #[test]
    fn test_update_input_checks() {
        let desc = format!("tr([73c5da0a/86'/0'/0']xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/0)");
        let desc = Descriptor::<DescriptorPublicKey>::from_str(&desc).unwrap();

        let mut non_witness_utxo = bitcoin::Transaction {
            version: 1,
            lock_time: 0,
            input: vec![],
            output: vec![TxOut {
                value: 1_000,
                script_pubkey: Script::from_str(
                    "5120a60869f0dbcf1dc659c9cecbaf8050135ea9e8cdc487053f1dc6880949dc684c",
                )
                .unwrap(),
            }],
        };

        let tx = bitcoin::Transaction {
            version: 1,
            lock_time: 0,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: non_witness_utxo.txid(),
                    vout: 0,
                },
                ..Default::default()
            }],
            output: vec![],
        };

        let mut psbt = Psbt::from_unsigned_tx(tx.clone()).unwrap();
        assert_eq!(
            psbt.update_input_with_descriptor(0, &desc),
            Err(UtxoUpdateError::UtxoCheck),
            "neither *_utxo are not set"
        );
        psbt.inputs[0].witness_utxo = Some(non_witness_utxo.output[0].clone());
        assert_eq!(
            psbt.update_input_with_descriptor(0, &desc),
            Ok(()),
            "witness_utxo is set which is ok"
        );
        psbt.inputs[0].non_witness_utxo = Some(non_witness_utxo.clone());
        assert_eq!(
            psbt.update_input_with_descriptor(0, &desc),
            Ok(()),
            "matching non_witness_utxo"
        );
        non_witness_utxo.version = 0;
        psbt.inputs[0].non_witness_utxo = Some(non_witness_utxo.clone());
        assert_eq!(
            psbt.update_input_with_descriptor(0, &desc),
            Err(UtxoUpdateError::UtxoCheck),
            "non_witness_utxo no longer matches"
        );
        psbt.inputs[0].non_witness_utxo = None;
        psbt.inputs[0].witness_utxo.as_mut().unwrap().script_pubkey = Script::default();
        assert_eq!(
            psbt.update_input_with_descriptor(0, &desc),
            Err(UtxoUpdateError::MismatchedScriptPubkey),
            "non_witness_utxo no longer matches"
        );
    }
}
