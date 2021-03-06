use bellman::groth16::{
    create_random_proof, verify_proof, Parameters, PreparedVerifyingKey, Proof,
};
use byteorder::{LittleEndian, ReadBytesExt};
use pairing::{
    bls12_381::{Bls12, Fr, FrRepr},
    Field, PrimeField, PrimeFieldRepr,
};
use rand::{OsRng, Rand};
use sapling_crypto::{
    circuit::{
        multipack,
        sapling::{Output, Spend, TREE_DEPTH},
    },
    jubjub::{edwards, fs::Fs, FixedGenerators, JubjubBls12, Unknown},
    jubjub::{fs::FsRepr, JubjubEngine},
    primitives::{Diversifier, Note, PaymentAddress, ProofGenerationKey, ValueCommitment},
    redjubjub::{PrivateKey, PublicKey, Signature},
};

fn read_fs(from: &[u8]) -> FsRepr {
    assert_eq!(from.len(), 32);

    let mut f = <<Bls12 as JubjubEngine>::Fs as PrimeField>::Repr::default();
    f.read_le(from).expect("length is 32 bytes");

    f
}
use super::compute_value_balance;

/// A witness to a path from a postion in a particular Sapling commitment tree
/// to the root of that tree.
pub struct CommitmentTreeWitness {
    auth_path: Vec<Option<(Fr, bool)>>,
    position: u64,
}

impl CommitmentTreeWitness {
    pub fn from_slice(mut witness: &[u8]) -> Result<Self, ()> {
        // Skip the first byte, which should be "32" to signify the length of
        // the following vector of Pedersen hashes.
        assert_eq!(witness[0], TREE_DEPTH as u8);
        witness = &witness[1..];

        // Begin to construct the authentication path
        let mut auth_path = vec![None; TREE_DEPTH];

        // The vector works in reverse
        for i in (0..TREE_DEPTH).rev() {
            // skip length of inner vector
            assert_eq!(witness[0], 32); // the length of a pedersen hash
            witness = &witness[1..];

            // Grab the sibling node at this depth in the tree
            let mut sibling = [0u8; 32];
            sibling.copy_from_slice(&witness[0..32]);
            witness = &witness[32..];

            // Sibling node should be an element of Fr
            let sibling = match {
                let mut repr = FrRepr::default();
                repr.read_le(&sibling[..]).expect("length is 32 bytes");
                Fr::from_repr(repr)
            } {
                Ok(p) => p,
                Err(_) => return Err(()),
            };

            // Set the value in the auth path; we put false here
            // for now (signifying the position bit) which we'll
            // fill in later.
            auth_path[i] = Some((sibling, false));
        }

        // Read the position from the witness
        let position = witness
            .read_u64::<LittleEndian>()
            .expect("should have had index at the end");

        // Given the position, let's finish constructing the authentication
        // path
        let mut tmp = position;
        for i in 0..TREE_DEPTH {
            auth_path[i].as_mut().map(|p| p.1 = (tmp & 1) == 1);

            tmp >>= 1;
        }

        // The witness should be empty now; if it wasn't, the caller would
        // have provided more information than they should have, indicating
        // a bug downstream
        assert_eq!(witness.len(), 0);

        Ok(CommitmentTreeWitness {
            auth_path,
            position,
        })
    }
}

/// A context object for creating the Sapling components of a Zcash transaction.
pub struct SaplingProvingContext {
    bsk: Fs,
    bvk: edwards::Point<Bls12, Unknown>,
}

impl SaplingProvingContext {
    /// Construct a new context to be used with a single transaction.
    pub fn new() -> Self {
        SaplingProvingContext {
            bsk: Fs::zero(),
            bvk: edwards::Point::zero(),
        }
    }

    /// Create the value commitment, re-randomized key, and proof for a Sapling
    /// SpendDescription, while accumulating its value commitment randomness
    /// inside the context for later use.
    pub fn spend_proof(
        &mut self,
        proof_generation_key: ProofGenerationKey<Bls12>,
        diversifier: Diversifier,
        rcm: Fs,
        ar: Fs,
        value: u64,
        anchor: Fr,
        witness: CommitmentTreeWitness,
        proving_key: &Parameters<Bls12>,
        verifying_key: &PreparedVerifyingKey<Bls12>,
        params: &JubjubBls12,
    ) -> Result<
        (
            Proof<Bls12>,
            edwards::Point<Bls12, Unknown>,
            PublicKey<Bls12>,
        ),
        (),
    > {
        // Initialize secure RNG
        let mut rng = OsRng::new().expect("should be able to construct RNG");

        // We create the randomness of the value commitment
        let rcv = Fs::rand(&mut rng);

        // Accumulate the value commitment randomness in the context
        {
            let mut tmp = rcv.clone();
            tmp.add_assign(&self.bsk);

            // Update the context
            self.bsk = tmp;
        }

        // Construct the value commitment
        let value_commitment = ValueCommitment::<Bls12> {
            value: value,
            randomness: rcv,
        };

        // Construct the viewing key
        let viewing_key = proof_generation_key.into_viewing_key(params);

        // Construct the payment address with the viewing key / diversifier
        let payment_address = match viewing_key.into_payment_address(diversifier, params) {
            Some(p) => p,
            None => return Err(()),
        };

        let mut ar_vec = [0u8; 32];
        ar.clone().into_repr().write_le(&mut ar_vec[0..32]);

        let mut data_to_be_signed = [0u8; 32];
        proof_generation_key
            .ak
            .clone()
            .write(&mut data_to_be_signed[0..32])
            .expect("message buffer should be 32 bytes");

        // This is the result of the re-randomization, we compute it for the caller
        let rk = PublicKey::<Bls12>(proof_generation_key.ak.clone().into()).randomize(
            ar,
            FixedGenerators::SpendingKeyGenerator,
            params,
        );
        ///////

        //////////

        // Let's compute the nullifier while we have the position
        let note = Note {
            value: value,
            g_d: diversifier
                .g_d::<Bls12>(params)
                .expect("was a valid diversifier before"),
            pk_d: payment_address.pk_d.clone(),
            r: rcm,
        };

        let nullifier = note.nf(&viewing_key, witness.position, params);

        // We now have the full witness for our circuit
        let instance = Spend {
            params,
            value_commitment: Some(value_commitment.clone()),
            proof_generation_key: Some(proof_generation_key),
            payment_address: Some(payment_address),
            commitment_randomness: Some(rcm),
            ar: Some(ar),
            auth_path: witness.auth_path,
            anchor: Some(anchor),
        };

        // Create proof
        let proof =
            create_random_proof(instance, proving_key, &mut rng).expect("proving should not fail");

        // Try to verify the proof:
        // Construct public input for circuit
        let mut public_input = [Fr::zero(); 7];
        {
            let (x, y) = rk.0.into_xy();
            public_input[0] = x;
            public_input[1] = y;
        }
        {
            let (x, y) = value_commitment.cm(params).into_xy();
            public_input[2] = x;
            public_input[3] = y;
        }
        public_input[4] = anchor;

        // Add the nullifier through multiscalar packing
        {
            let nullifier = multipack::bytes_to_bits_le(&nullifier);
            let nullifier = multipack::compute_multipacking::<Bls12>(&nullifier);

            assert_eq!(nullifier.len(), 2);

            public_input[5] = nullifier[0];
            public_input[6] = nullifier[1];
        }

        // Verify the proof
        match verify_proof(verifying_key, &proof, &public_input[..]) {
            // No error, and proof verification successful
            Ok(true) => {}

            // Any other case
            _ => {
                return Err(());
            }
        }

        // Compute value commitment
        let value_commitment: edwards::Point<Bls12, Unknown> = value_commitment.cm(params).into();

        // Accumulate the value commitment in the context
        {
            let mut tmp = value_commitment.clone();
            tmp = tmp.add(&self.bvk, params);

            // Update the context
            self.bvk = tmp;
        }

        Ok((proof, value_commitment, rk))
    }

    /// Create the value commitment and proof for a Sapling OutputDescription,
    /// while accumulating its value commitment randomness inside the context
    /// for later use.
    pub fn output_proof(
        &mut self,
        esk: Fs,
        payment_address: PaymentAddress<Bls12>,
        rcm: Fs,
        value: u64,
        proving_key: &Parameters<Bls12>,
        params: &JubjubBls12,
    ) -> (Proof<Bls12>, edwards::Point<Bls12, Unknown>) {
        // Initialize secure RNG
        let mut rng = OsRng::new().expect("should be able to construct RNG");

        // We construct ephemeral randomness for the value commitment. This
        // randomness is not given back to the caller, but the synthetic
        // blinding factor `bsk` is accumulated in the context.
        let rcv = Fs::rand(&mut rng);

        // Accumulate the value commitment randomness in the context
        {
            let mut tmp = rcv.clone();
            tmp.negate(); // Outputs subtract from the total.
            tmp.add_assign(&self.bsk);

            // Update the context
            self.bsk = tmp;
        }

        // Construct the value commitment for the proof instance
        let value_commitment = ValueCommitment::<Bls12> {
            value: value,
            randomness: rcv,
        };

        // We now have a full witness for the output proof.
        let instance = Output {
            params,
            value_commitment: Some(value_commitment.clone()),
            payment_address: Some(payment_address.clone()),
            commitment_randomness: Some(rcm),
            esk: Some(esk.clone()),
        };

        // Create proof
        let proof =
            create_random_proof(instance, proving_key, &mut rng).expect("proving should not fail");

        // Compute the actual value commitment
        let value_commitment: edwards::Point<Bls12, Unknown> = value_commitment.cm(params).into();

        // Accumulate the value commitment in the context. We do this to check internal consistency.
        {
            let mut tmp = value_commitment.clone();
            tmp = tmp.negate(); // Outputs subtract from the total.
            tmp = tmp.add(&self.bvk, params);

            // Update the context
            self.bvk = tmp;
        }

        (proof, value_commitment)
    }

    /// Create the bindingSig for a Sapling transaction. All calls to spend_proof()
    /// and output_proof() must be completed before calling this function.
    pub fn binding_sig(
        &self,
        value_balance: i64,
        sighash: &[u8; 32],
        params: &JubjubBls12,
    ) -> Result<Signature, ()> {
        // Initialize secure RNG
        let mut rng = OsRng::new().expect("should be able to construct RNG");

        // Grab the current `bsk` from the context
        let bsk = PrivateKey::<Bls12>(self.bsk);

        // Grab the `bvk` using DerivePublic.
        let bvk = PublicKey::from_private(&bsk, FixedGenerators::ValueCommitmentRandomness, params);

        // In order to check internal consistency, let's use the accumulated value
        // commitments (as the verifier would) and apply valuebalance to compare
        // against our derived bvk.
        {
            // Compute value balance
            let mut value_balance = match compute_value_balance(value_balance, params) {
                Some(a) => a,
                None => return Err(()),
            };

            // Subtract value_balance from current bvk to get final bvk
            value_balance = value_balance.negate();
            let mut tmp = self.bvk.clone();
            tmp = tmp.add(&value_balance, params);

            // The result should be the same, unless the provided valueBalance is wrong.
            if bvk.0 != tmp {
                return Err(());
            }
        }

        // Construct signature message
        let mut data_to_be_signed = [0u8; 64];
        bvk.0
            .write(&mut data_to_be_signed[0..32])
            .expect("message buffer should be 32 bytes");
        (&mut data_to_be_signed[32..64]).copy_from_slice(&sighash[..]);

        // Sign
        Ok(bsk.sign(
            &data_to_be_signed,
            &mut rng,
            FixedGenerators::ValueCommitmentRandomness,
            params,
        ))
    }
}
