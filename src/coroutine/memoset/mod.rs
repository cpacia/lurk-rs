//! The `memoset` module implements a `MemoSet`.
//!
//! A `MemoSet` is an abstraction we use to memoize deferred proof of (potentially mutually-recursive) query results.
//! Whenever a computation being proved needs the result of a query, the prover non-deterministically supplies the
//! correct response. The resulting key-value pair is then added to a multiset representing deferred proofs. The
//! dependent proof now must not be accepted until every element in the deferred-proof multiset has been proved.
//!
//! Implementation depends on a cryptographic multiset -- for example, ECMH or LogUp (implemented here). This allows us
//! to prove that every element added to to the multiset is later removed only after having been proved. The
//! cryptographic assumption is that it is infeasible to fraudulently demonstrate multiset equality.
//!
//! Our use of the LogUp (logarithmic derivative) technique in the `LogMemo` implementation of `MemoSet` unfortunately
//! requires that the entire history of insertions and removals be committed to in advance -- so that Fiat-Shamir
//! randomness derived from the transcript can be used when mapping field elements to multiset elements. We use Lurk
//! data to assemble the transcript, so that the final randomness is the hash/value component of a `ZPtr` to the
//! content-addressed data structure representing the transcript as assembled.
//!
//! Transcript elements represent deferred proofs that are either added to (when their results are used) or removed from
//! (when correctness of those results is proved) the 'deferred proof' multiset. Insertions are recorded in the
//! transcript as key-value pairs (Lurk data: `(key . value)`); and removals further include the removal multiplicity
//! (Lurk data: `((key . value) . multiplicity)`). It is critical that the multiplicity be included in the transcript,
//! since if free to choose it after the randomness has been derived, the prover can trivially falsify the contents of
//! the multiset -- decoupling claimed truths from those actually proved.
//!
//! Bookkeeping required to correctly build the transcript after evaluation but before proving is maintained by the
//! `Scope`. This allows us to accumulate queries and the subqueries on which they depend, along with the memoized query
//! results computed 'naturally' during evaluation. We then separate and sort in an order matching that which the NIVC
//! prover will follow when provably maintaining the multiset accumulator and Fiat-Shamir transcript in the circuit.

use itertools::Itertools;
use std::collections::HashMap;
use std::marker::PhantomData;

use bellpepper_core::{boolean::Boolean, num::AllocatedNum, ConstraintSystem, SynthesisError};
use indexmap::IndexSet;
use once_cell::sync::OnceCell;

use crate::circuit::gadgets::{
    constraints::{enforce_equal, enforce_equal_zero, invert, sub},
    pointer::AllocatedPtr,
};
use crate::coprocessor::gadgets::construct_cons; // FIXME: Move to common location.
use crate::field::LurkField;
use crate::lem::circuit::GlobalAllocator;
use crate::lem::tag::Tag;
use crate::lem::{pointers::Ptr, store::Store};
use crate::tag::{ExprTag, Tag as XTag};
use crate::z_ptr::ZPtr;

use multiset::MultiSet;
pub use query::{CircuitQuery, Query};

mod demo;
mod env;
mod multiset;
mod query;

#[derive(Clone, Debug)]
pub struct Transcript<F> {
    acc: Ptr,
    _p: PhantomData<F>,
}

impl<F: LurkField> Transcript<F> {
    fn new(s: &Store<F>) -> Self {
        let nil = s.intern_nil();
        Self {
            acc: nil,
            _p: Default::default(),
        }
    }

    fn add(&mut self, s: &Store<F>, item: Ptr) {
        self.acc = s.cons(item, self.acc);
    }

    fn make_kv(s: &Store<F>, key: Ptr, value: Ptr) -> Ptr {
        s.cons(key, value)
    }

    fn make_kv_count(s: &Store<F>, kv: Ptr, count: usize) -> Ptr {
        let count_num = s.num(F::from_u64(count as u64));
        s.cons(kv, count_num)
    }

    /// Since the transcript is just a content-addressed Lurk list, its randomness is the hash value of the associated
    /// top-level `Cons`. This function sanity-checks the type and extracts that field element.
    fn r(&self, s: &Store<F>) -> F {
        let z_ptr = s.hash_ptr(&self.acc);
        assert_eq!(Tag::Expr(ExprTag::Cons), *z_ptr.tag());
        *z_ptr.value()
    }

    #[allow(dead_code)]
    fn dbg(&self, s: &Store<F>) {
        tracing::debug!("transcript: {}", self.acc.fmt_to_string_simple(s));
    }

    #[allow(dead_code)]
    fn fmt_to_string_simple(&self, s: &Store<F>) -> String {
        self.acc.fmt_to_string_simple(s)
    }
}

#[derive(Clone, Debug)]
pub struct CircuitTranscript<F: LurkField> {
    acc: AllocatedPtr<F>,
}

impl<F: LurkField> CircuitTranscript<F> {
    fn new<CS: ConstraintSystem<F>>(cs: &mut CS, g: &mut GlobalAllocator<F>, s: &Store<F>) -> Self {
        let nil = s.intern_nil();
        let allocated_nil = g.alloc_ptr(cs, &nil, s);
        Self {
            acc: allocated_nil.clone(),
        }
    }

    pub fn pick<CS: ConstraintSystem<F>>(
        cs: &mut CS,
        condition: &Boolean,
        a: &Self,
        b: &Self,
    ) -> Result<Self, SynthesisError> {
        let picked = AllocatedPtr::pick(cs, condition, &a.acc, &b.acc)?;
        Ok(Self { acc: picked })
    }

    fn add<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        s: &Store<F>,
        item: &AllocatedPtr<F>,
    ) -> Result<Self, SynthesisError> {
        let acc = construct_cons(cs, g, s, item, &self.acc)?;

        Ok(Self { acc })
    }

    fn make_kv<CS: ConstraintSystem<F>>(
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        s: &Store<F>,
        key: &AllocatedPtr<F>,
        value: &AllocatedPtr<F>,
    ) -> Result<AllocatedPtr<F>, SynthesisError> {
        construct_cons(cs, g, s, key, value)
    }

    fn make_kv_count<CS: ConstraintSystem<F>>(
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        s: &Store<F>,
        kv: &AllocatedPtr<F>,
        count: u64,
    ) -> Result<(AllocatedPtr<F>, AllocatedNum<F>), SynthesisError> {
        let allocated_count =
            { AllocatedNum::alloc(&mut cs.namespace(|| "count"), || Ok(F::from_u64(count)))? };
        let count_ptr = AllocatedPtr::alloc_tag(
            &mut cs.namespace(|| "count_ptr"),
            ExprTag::Num.to_field(),
            allocated_count.clone(),
        )?;

        Ok((construct_cons(cs, g, s, kv, &count_ptr)?, allocated_count))
    }

    fn r(&self) -> &AllocatedNum<F> {
        self.acc.hash()
    }

    #[allow(dead_code)]
    fn dbg(&self, s: &Store<F>) {
        let z = self.acc.get_value::<Tag>().unwrap();
        let transcript = s.to_ptr(&z);
        tracing::debug!("transcript: {}", transcript.fmt_to_string_simple(s));
    }
}

#[derive(Clone, Debug)]
/// A `Scope` tracks the queries made while evaluating, including the subqueries that result from evaluating other
/// queries -- then makes use of the bookkeeping performed at evaluation time to synthesize proof of each query
/// performed.
pub struct Scope<Q, M> {
    memoset: M,
    /// k => v
    queries: HashMap<Ptr, Ptr>,
    /// k => ordered subqueries
    dependencies: HashMap<Ptr, Vec<Q>>,
    /// kv pairs
    toplevel_insertions: Vec<Ptr>,
    /// internally-inserted keys
    internal_insertions: Vec<Ptr>,
    /// unique keys: query-index -> [key]
    unique_inserted_keys: HashMap<usize, Vec<Ptr>>,
    transcribe_internal_insertions: bool,
    // This may become an explicit map or something allowing more fine-grained control.
    default_rc: usize,
}

const DEFAULT_RC_FOR_QUERY: usize = 1;
const DEFAULT_TRANSCRIBE_INTERNAL_INSERTIONS: bool = false;

impl<F: LurkField, Q> Default for Scope<Q, LogMemo<F>> {
    fn default() -> Self {
        Self::new(DEFAULT_TRANSCRIBE_INTERNAL_INSERTIONS, DEFAULT_RC_FOR_QUERY)
    }
}

impl<F: LurkField, Q> Scope<Q, LogMemo<F>> {
    fn new(transcribe_internal_insertions: bool, default_rc: usize) -> Self {
        Self {
            memoset: Default::default(),
            queries: Default::default(),
            dependencies: Default::default(),
            toplevel_insertions: Default::default(),
            internal_insertions: Default::default(),
            unique_inserted_keys: Default::default(),
            transcribe_internal_insertions,
            default_rc,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CircuitScope<F: LurkField, CM> {
    memoset: CM, // CircuitMemoSet
    /// k -> v
    queries: HashMap<ZPtr<Tag, F>, ZPtr<Tag, F>>,
    /// k -> allocated v
    transcript: CircuitTranscript<F>,
    acc: Option<AllocatedPtr<F>>,
    transcribe_internal_insertions: bool,
}

pub struct CoroutineCircuit<'a, F: LurkField, CM, Q> {
    queries: &'a HashMap<Ptr, Ptr>,
    memoset: CM,
    keys: Vec<Ptr>,
    query_index: usize,
    store: &'a Store<F>,
    transcribe_internal_insertions: bool,
    rc: usize,
    _p: PhantomData<Q>,
}

// TODO: Make this generic rather than specialized to LogMemo.
// That will require a CircuitScopeTrait.
impl<'a, F: LurkField, Q: Query<F>> CoroutineCircuit<'a, F, LogMemoCircuit<F>, Q> {
    fn new(
        scope: &'a Scope<Q, LogMemo<F>>,
        memoset: LogMemoCircuit<F>,
        keys: Vec<Ptr>,
        query_index: usize,
        store: &'a Store<F>,
        rc: usize,
    ) -> Self {
        assert!(keys.len() <= rc);
        Self {
            memoset,
            queries: &scope.queries,
            keys,
            query_index,
            store,
            transcribe_internal_insertions: scope.transcribe_internal_insertions,
            rc,
            _p: Default::default(),
        }
    }

    // This is a supernova::StepCircuit method.
    // // TODO: we need to create a supernova::StepCircuit that will prove up to a fixed number of queries of a given type.
    fn synthesize<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        z: &[AllocatedPtr<F>],
    ) -> Result<(Option<AllocatedNum<F>>, Vec<AllocatedPtr<F>>), SynthesisError> {
        let g = &mut GlobalAllocator::<F>::default();

        assert_eq!(6, z.len());
        let [c, e, k, memoset_acc, transcript, r] = z else {
            unreachable!()
        };

        let mut circuit_scope: CircuitScope<F, LogMemoCircuit<F>> = CircuitScope::from_queries(
            cs,
            g,
            self.store,
            self.memoset.clone(),
            self.queries,
            self.transcribe_internal_insertions,
        );
        circuit_scope.update_from_io(memoset_acc.clone(), transcript.clone(), r);

        for (i, key) in self
            .keys
            .iter()
            .map(Some)
            .pad_using(self.rc, |_| None)
            .enumerate()
        {
            let cs = &mut cs.namespace(|| format!("internal-{i}"));
            circuit_scope.synthesize_prove_key_query::<_, Q>(
                cs,
                g,
                self.store,
                key,
                self.query_index,
            )?;
        }

        let (memoset_acc, transcript, r_num) = circuit_scope.io();
        let r = AllocatedPtr::alloc_tag(&mut cs.namespace(|| "r"), ExprTag::Num.to_field(), r_num)?;

        let z_out = vec![c.clone(), e.clone(), k.clone(), memoset_acc, transcript, r];

        let next_pc = None; // FIXME.
        Ok((next_pc, z_out))
    }
}

impl<F: LurkField, Q: Query<F>> Scope<Q, LogMemo<F>> {
    pub fn query(&mut self, s: &Store<F>, form: Ptr) -> Ptr {
        let (response, kv_ptr) = self.query_aux(s, form);

        self.toplevel_insertions.push(kv_ptr);

        response
    }

    fn query_recursively(&mut self, s: &Store<F>, parent: &Q, child: Q) -> Ptr {
        let form = child.to_ptr(s);
        self.internal_insertions.push(form);

        let (response, _) = self.query_aux(s, form);

        self.dependencies
            .entry(parent.to_ptr(s))
            .and_modify(|children| children.push(child.clone()))
            .or_insert_with(|| vec![child]);

        response
    }

    fn query_aux(&mut self, s: &Store<F>, form: Ptr) -> (Ptr, Ptr) {
        let response = self.queries.get(&form).cloned().unwrap_or_else(|| {
            let query = Q::from_ptr(s, &form).expect("invalid query");

            let evaluated = query.eval(s, self);

            self.queries.insert(form, evaluated);
            evaluated
        });

        let kv = Transcript::make_kv(s, form, response);
        self.memoset.add(kv);

        (response, kv)
    }

    fn finalize_transcript(&mut self, s: &Store<F>) -> Transcript<F> {
        let (transcript, insertions) = self.build_transcript(s);
        self.memoset.finalize_transcript(s, transcript.clone());
        self.unique_inserted_keys = insertions;
        transcript
    }

    fn ensure_transcript_finalized(&mut self, s: &Store<F>) {
        if !self.memoset.is_finalized() {
            self.finalize_transcript(s);
        }
    }

    fn build_transcript(&self, s: &Store<F>) -> (Transcript<F>, HashMap<usize, Vec<Ptr>>) {
        let mut transcript = Transcript::new(s);

        // k -> [kv]
        let mut insertions: HashMap<Ptr, IndexSet<Ptr>> = HashMap::new();
        let mut unique_keys: HashMap<usize, Vec<Ptr>> = Default::default();

        let mut insert = |kv: Ptr| {
            let key = s.car_cdr(&kv).unwrap().0;

            if let Some(kvs) = insertions.get_mut(&key) {
                kvs.insert(kv);
            } else {
                let index = Q::from_ptr(s, &key).expect("bad query").index();
                unique_keys
                    .entry(index)
                    .and_modify(|keys| keys.push(key))
                    .or_insert_with(|| vec![key]);
                let mut x = IndexSet::new();
                x.insert(kv);

                insertions.insert(key, x);
            }
        };

        let internal_insertions_kv = self.internal_insertions.iter().map(|key| {
            let value = self.queries.get(key).expect("value missing for key");
            Transcript::make_kv(s, *key, *value)
        });

        for kv in &self.toplevel_insertions {
            insert(*kv);
        }
        for kv in internal_insertions_kv {
            insert(kv);
        }
        for kv in self.toplevel_insertions.iter() {
            transcript.add(s, *kv);
        }

        // Then add insertions and removals interleaved, sorted by query type. We interleave insertions and removals
        // because when proving later, each query's proof must record that its subquery proofs are being deferred
        // (insertions) before then proving itself (making use of any subquery results) and removing the now-proved
        // deferral from the MemoSet.
        for index in 0..Q::count() {
            for key in unique_keys.get(&index).expect("unreachable") {
                for kv in insertions.get(key).unwrap().iter() {
                    if let Some(dependencies) = self.dependencies.get(key) {
                        dependencies.iter().for_each(|dependency| {
                            let k = dependency.to_ptr(s);
                            let v = self
                                .queries
                                .get(&k)
                                .expect("value missing for dependency key");
                            // Add an insertion for each dependency (subquery) of the query identified by `key`. Notice
                            // that these keys might already have been inserted before, but we need to repeat if so
                            // because the proof must do so each time a query is used.
                            let kv = Transcript::make_kv(s, k, *v);
                            if self.transcribe_internal_insertions {
                                transcript.add(s, kv)
                            }
                        })
                    };
                    let count = self.memoset.count(kv);
                    let kv_count = Transcript::make_kv_count(s, *kv, count);

                    // Add removal for the query identified by `key`. The queries being removed here were deduplicated
                    // above, so each is removed only once. However, we freely choose the multiplicity (`count`) of the
                    // removal to match the total number of insertions actually made (considering dependencies).
                    transcript.add(s, kv_count);
                }
            }
        }
        (transcript, unique_keys)
    }

    pub fn synthesize<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
    ) -> Result<(), SynthesisError> {
        self.ensure_transcript_finalized(s);
        // FIXME: Do we need to allocate a new GlobalAllocator here?
        // Is it okay for this memoset circuit to be shared between all CoroutineCircuits?
        let memoset_circuit = self
            .memoset
            .to_circuit(&mut cs.namespace(|| "memoset_circuit"));

        let mut circuit_scope = CircuitScope::from_queries(
            &mut cs.namespace(|| "transcript"),
            g,
            s,
            memoset_circuit.clone(),
            &self.queries,
            self.transcribe_internal_insertions,
        );
        circuit_scope.init(cs, g, s);
        {
            circuit_scope.synthesize_insert_toplevel_queries(self, cs, g, s)?;

            {
                let (memoset_acc, transcript, r_num) = circuit_scope.io();
                let r = AllocatedPtr::alloc_tag(
                    &mut cs.namespace(|| "r"),
                    ExprTag::Num.to_field(),
                    r_num,
                )?;
                let dummy = g.alloc_ptr(cs, &s.intern_nil(), s);
                let mut z = vec![
                    dummy.clone(),
                    dummy.clone(),
                    dummy.clone(),
                    memoset_acc,
                    transcript,
                    r,
                ];
                for (index, keys) in self.unique_inserted_keys.iter() {
                    let cs = &mut cs.namespace(|| format!("query-index-{index}"));

                    let rc = self.rc_for_query(*index);

                    for (i, chunk) in keys.chunks(rc).enumerate() {
                        // This namespace exists only because we are putting multiple 'chunks' into a single, larger circuit (as a stage in development).
                        // It shouldn't exist, when instead we have only the single NIVC circuit repeated multiple times.
                        let cs = &mut cs.namespace(|| format!("chunk-{i}"));

                        let mut circuit: CoroutineCircuit<'_, F, LogMemoCircuit<F>, Q> =
                            CoroutineCircuit::new(
                                self,
                                memoset_circuit.clone(),
                                chunk.to_vec(),
                                *index,
                                s,
                                rc,
                            );

                        let (_next_pc, z_out) = circuit.synthesize(cs, &z)?;
                        {
                            let memoset_acc = &z_out[3];
                            let transcript = &z_out[4];
                            let r = &z_out[5];

                            circuit_scope.update_from_io(
                                memoset_acc.clone(),
                                transcript.clone(),
                                r,
                            );

                            z = z_out;
                        }
                    }
                }
            }
        }

        circuit_scope.finalize(cs, g);

        Ok(())
    }

    fn rc_for_query(&self, _index: usize) -> usize {
        self.default_rc
    }
}

impl<F: LurkField> CircuitScope<F, LogMemoCircuit<F>> {
    fn from_queries<CS: ConstraintSystem<F>>(
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
        memoset: LogMemoCircuit<F>,
        queries: &HashMap<Ptr, Ptr>,
        transcribe_internal_insertions: bool,
    ) -> Self {
        let queries = queries
            .iter()
            .map(|(k, v)| (s.hash_ptr(k), s.hash_ptr(v)))
            .collect();

        Self {
            memoset,
            queries,
            transcript: CircuitTranscript::new(cs, g, s),
            acc: Default::default(),
            transcribe_internal_insertions,
        }
    }

    fn init<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
    ) {
        self.acc = Some(
            AllocatedPtr::alloc_constant(&mut cs.namespace(|| "acc"), s.hash_ptr(&s.num_u64(0)))
                .unwrap(),
        );

        self.transcript = CircuitTranscript::new(cs, g, s);
    }

    fn io(&self) -> (AllocatedPtr<F>, AllocatedPtr<F>, AllocatedNum<F>) {
        (
            self.acc.as_ref().unwrap().clone(),
            self.transcript.acc.clone(),
            self.memoset.r.clone(),
        )
    }

    fn update_from_io(
        &mut self,
        acc: AllocatedPtr<F>,
        transcript: AllocatedPtr<F>,
        r: &AllocatedPtr<F>,
    ) {
        self.acc = Some(acc);
        self.transcript.acc = transcript;
        self.memoset.r = r.hash().clone();
    }

    fn synthesize_insert_query<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        s: &Store<F>,
        acc: &AllocatedPtr<F>,
        transcript: &CircuitTranscript<F>,
        key: &AllocatedPtr<F>,
        value: &AllocatedPtr<F>,
        is_toplevel: bool,
    ) -> Result<(AllocatedPtr<F>, CircuitTranscript<F>), SynthesisError> {
        let kv = CircuitTranscript::make_kv(&mut cs.namespace(|| "kv"), g, s, key, value)?;
        let new_transcript = if is_toplevel || self.transcribe_internal_insertions {
            transcript.add(&mut cs.namespace(|| "new_transcript"), g, s, &kv)?
        } else {
            transcript.clone()
        };

        let acc_v = acc.hash();

        let new_acc_v =
            self.memoset
                .synthesize_add(&mut cs.namespace(|| "new_acc_v"), acc_v, &kv)?;

        let new_acc = AllocatedPtr::alloc_tag(
            &mut cs.namespace(|| "new_acc"),
            ExprTag::Num.to_field(),
            new_acc_v,
        )?;

        Ok((new_acc, new_transcript.clone()))
    }

    fn synthesize_remove<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        s: &Store<F>,
        acc: &AllocatedPtr<F>,
        transcript: &CircuitTranscript<F>,
        key: &AllocatedPtr<F>,
        value: &AllocatedPtr<F>,
    ) -> Result<(AllocatedPtr<F>, CircuitTranscript<F>), SynthesisError> {
        let kv = CircuitTranscript::make_kv(&mut cs.namespace(|| "kv"), g, s, key, value)?;
        let zptr = kv.get_value().unwrap_or(s.hash_ptr(&s.intern_nil())); // dummy case: use nil
        let raw_count = self.memoset.count(&s.to_ptr(&zptr)) as u64; // dummy case: count is meaningless

        let (kv_count, count) = CircuitTranscript::make_kv_count(
            &mut cs.namespace(|| "kv_count"),
            g,
            s,
            &kv,
            raw_count,
        )?;
        let new_transcript = transcript.add(
            &mut cs.namespace(|| "new_removal_transcript"),
            g,
            s,
            &kv_count,
        )?;

        let new_acc_v = self.memoset.synthesize_remove_n(
            &mut cs.namespace(|| "new_acc_v"),
            acc.hash(),
            &kv,
            &count,
        )?;

        let new_acc = AllocatedPtr::alloc_tag(
            &mut cs.namespace(|| "new_acc"),
            ExprTag::Num.to_field(),
            new_acc_v,
        )?;
        Ok((new_acc, new_transcript))
    }

    fn finalize<CS: ConstraintSystem<F>>(&mut self, cs: &mut CS, _g: &mut GlobalAllocator<F>) {
        let r = self.memoset.allocated_r();
        enforce_equal(cs, || "r_matches_transcript", self.transcript.r(), &r);
        enforce_equal_zero(cs, || "acc_is_zero", self.acc.clone().unwrap().hash());
    }

    fn synthesize_query<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        store: &Store<F>,
        key: &AllocatedPtr<F>,
        acc: &AllocatedPtr<F>,
        transcript: &CircuitTranscript<F>,
        not_dummy: &Boolean,
    ) -> Result<(AllocatedPtr<F>, AllocatedPtr<F>, CircuitTranscript<F>), SynthesisError> {
        self.synthesize_query_aux(cs, g, store, key, acc, transcript, not_dummy, true)
    }

    fn synthesize_internal_query<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        store: &Store<F>,
        key: &AllocatedPtr<F>,
        acc: &AllocatedPtr<F>,
        transcript: &CircuitTranscript<F>,
        not_dummy: &Boolean,
    ) -> Result<(AllocatedPtr<F>, AllocatedPtr<F>, CircuitTranscript<F>), SynthesisError> {
        self.synthesize_query_aux(cs, g, store, key, acc, transcript, not_dummy, false)
    }

    fn synthesize_query_aux<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &GlobalAllocator<F>,
        store: &Store<F>,
        key: &AllocatedPtr<F>,
        acc: &AllocatedPtr<F>,
        transcript: &CircuitTranscript<F>,
        not_dummy: &Boolean, // TODO: use this more deeply?
        is_toplevel: bool,
    ) -> Result<(AllocatedPtr<F>, AllocatedPtr<F>, CircuitTranscript<F>), SynthesisError> {
        let value = AllocatedPtr::alloc(&mut cs.namespace(|| "value"), || {
            Ok(if not_dummy.get_value() == Some(true) {
                *key.get_value()
                    .and_then(|k| self.queries.get(&k))
                    .ok_or(SynthesisError::AssignmentMissing)?
            } else {
                // Dummy value that will not be used.
                store.hash_ptr(&store.intern_nil())
            })
        })?;

        let (new_acc, new_insertion_transcript) =
            self.synthesize_insert_query(cs, g, store, acc, transcript, key, &value, is_toplevel)?;

        Ok((value, new_acc, new_insertion_transcript))
    }

    fn synthesize_insert_toplevel_queries<CS: ConstraintSystem<F>, Q: Query<F>>(
        &mut self,
        scope: &mut Scope<Q, LogMemo<F>>,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
    ) -> Result<(), SynthesisError> {
        for (i, kv) in scope.toplevel_insertions.iter().enumerate() {
            self.synthesize_toplevel_query(cs, g, s, i, kv)?;
        }
        Ok(())
    }

    fn synthesize_toplevel_query<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
        i: usize,
        kv: &Ptr,
    ) -> Result<(), SynthesisError> {
        let (key, value) = s.car_cdr(kv).unwrap();
        let cs = &mut cs.namespace(|| format!("toplevel-{i}"));
        let allocated_key = AllocatedPtr::alloc(&mut cs.namespace(|| "allocated_key"), || {
            Ok(s.hash_ptr(&key))
        })
        .unwrap();

        let acc = self.acc.clone().unwrap();
        let insertion_transcript = self.transcript.clone();

        let (val, new_acc, new_transcript) = self.synthesize_query(
            cs,
            g,
            s,
            &allocated_key,
            &acc,
            &insertion_transcript,
            &Boolean::Constant(true),
        )?;

        if let Some(val_ptr) = val.get_value().map(|x| s.to_ptr(&x)) {
            assert_eq!(value, val_ptr);
        }

        self.acc = Some(new_acc);
        self.transcript = new_transcript;
        Ok(())
    }

    fn synthesize_prove_key_query<CS: ConstraintSystem<F>, Q: Query<F>>(
        &mut self,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
        key: Option<&Ptr>,
        index: usize,
    ) -> Result<(), SynthesisError> {
        let allocated_key = AllocatedPtr::alloc(&mut cs.namespace(|| "allocated_key"), || {
            if let Some(key) = key {
                Ok(s.hash_ptr(key))
            } else {
                Ok(s.hash_ptr(&s.intern_nil()))
            }
        })
        .unwrap();

        let circuit_query = if let Some(key) = key {
            Q::CQ::from_ptr(&mut cs.namespace(|| "circuit_query"), s, key).unwrap()
        } else {
            Q::CQ::dummy_from_index(&mut cs.namespace(|| "circuit_query"), s, index)
        };

        let not_dummy = key.is_some();

        self.synthesize_prove_query::<_, Q::CQ>(
            cs,
            g,
            s,
            &allocated_key,
            &circuit_query,
            not_dummy,
        )?;
        Ok(())
    }

    fn synthesize_prove_query<CS: ConstraintSystem<F>, CQ: CircuitQuery<F>>(
        &mut self,
        cs: &mut CS,
        g: &mut GlobalAllocator<F>,
        s: &Store<F>,
        allocated_key: &AllocatedPtr<F>,
        circuit_query: &CQ,
        not_dummy: bool,
    ) -> Result<(), SynthesisError> {
        let acc = self.acc.clone().unwrap();
        let transcript = self.transcript.clone();

        let (val, new_acc, new_transcript) = circuit_query
            .synthesize_eval(&mut cs.namespace(|| "eval"), g, s, self, &acc, &transcript)
            .unwrap();

        let (new_acc, new_transcript) =
            self.synthesize_remove(cs, g, s, &new_acc, &new_transcript, allocated_key, &val)?;

        // Prover can choose non-deterministically whether or not a given query is a dummy, to allow for padding.
        let final_acc = AllocatedPtr::pick(
            &mut cs.namespace(|| "final_acc"),
            &Boolean::Constant(not_dummy),
            &new_acc,
            self.acc.as_ref().expect("acc missing"),
        )?;
        let final_transcript = CircuitTranscript::pick(
            &mut cs.namespace(|| "final_transcripot"),
            &Boolean::Constant(not_dummy),
            &new_transcript,
            &self.transcript,
        )?;

        self.acc = Some(final_acc);
        self.transcript = final_transcript;

        Ok(())
    }

    #[allow(dead_code)]
    fn dbg_transcript(&self, s: &Store<F>) {
        self.transcript.dbg(s);
    }
}

pub trait CircuitMemoSet<F: LurkField>: Clone {
    fn synthesize_remove_n<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        acc: &AllocatedNum<F>,
        kv: &AllocatedPtr<F>,
        count: &AllocatedNum<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError>;

    fn allocated_r(&self) -> AllocatedNum<F>;

    // x is H(k,v) = hash part of (cons k v)
    fn synthesize_map_to_element<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        x: AllocatedNum<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError>;

    fn synthesize_add<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        acc: &AllocatedNum<F>,
        kv: &AllocatedPtr<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError>;

    fn count(&self, form: &Ptr) -> usize;
}

pub trait MemoSet<F: LurkField>: Clone {
    type CM: CircuitMemoSet<F>;

    fn into_circuit<CS: ConstraintSystem<F>>(self, cs: &mut CS) -> Self::CM;
    fn to_circuit<CS: ConstraintSystem<F>>(&self, cs: &mut CS) -> Self::CM;

    fn is_finalized(&self) -> bool;
    fn finalize_transcript(&mut self, s: &Store<F>, transcript: Transcript<F>);
    fn r(&self) -> Option<&F>;
    fn map_to_element(&self, x: F) -> Option<F>;
    fn add(&mut self, kv: Ptr);
    fn count(&self, form: &Ptr) -> usize;
}

#[derive(Debug, Clone)]
pub struct LogMemo<F: LurkField> {
    multiset: MultiSet<Ptr>,
    r: OnceCell<F>,
    transcript: OnceCell<Transcript<F>>,

    // Allocated only after transcript has been finalized.
    allocated_r: OnceCell<Option<AllocatedNum<F>>>,
}

#[derive(Debug, Clone)]
pub struct LogMemoCircuit<F: LurkField> {
    multiset: MultiSet<Ptr>,
    r: AllocatedNum<F>,
}

impl<F: LurkField> Default for LogMemo<F> {
    fn default() -> Self {
        // Be explicit.
        Self {
            multiset: MultiSet::new(),
            r: Default::default(),
            transcript: Default::default(),
            allocated_r: Default::default(),
        }
    }
}
impl<F: LurkField> LogMemo<F> {
    fn allocated_r<CS: ConstraintSystem<F>>(&self, cs: &mut CS) -> AllocatedNum<F> {
        self.allocated_r
            .get_or_init(|| {
                self.r()
                    .map(|r| AllocatedNum::alloc_infallible(&mut cs.namespace(|| "r"), || *r))
            })
            .clone()
            .unwrap()
    }
}

impl<F: LurkField> MemoSet<F> for LogMemo<F> {
    type CM = LogMemoCircuit<F>;

    fn into_circuit<CS: ConstraintSystem<F>>(self, cs: &mut CS) -> Self::CM {
        let r = self.allocated_r(cs);
        LogMemoCircuit {
            multiset: self.multiset,
            r,
        }
    }

    fn to_circuit<CS: ConstraintSystem<F>>(&self, cs: &mut CS) -> Self::CM {
        let r = self.allocated_r(cs);
        LogMemoCircuit {
            multiset: self.multiset.clone(),
            r,
        }
    }

    fn count(&self, form: &Ptr) -> usize {
        self.multiset.get(form).unwrap_or(0)
    }

    fn is_finalized(&self) -> bool {
        self.transcript.get().is_some()
    }
    fn finalize_transcript(&mut self, s: &Store<F>, transcript: Transcript<F>) {
        let r = transcript.r(s);

        self.r.set(r).expect("r has already been set");

        self.transcript
            .set(transcript)
            .expect("transcript already finalized");
    }

    fn r(&self) -> Option<&F> {
        self.r.get()
    }

    // x is H(k,v) = hash part of (cons k v)
    fn map_to_element(&self, x: F) -> Option<F> {
        self.r().and_then(|r| {
            let d = *r + x;
            d.invert().into()
        })
    }

    fn add(&mut self, kv: Ptr) {
        self.multiset.add(kv);
    }
}

impl<F: LurkField> CircuitMemoSet<F> for LogMemoCircuit<F> {
    fn allocated_r(&self) -> AllocatedNum<F> {
        self.r.clone()
    }

    fn synthesize_add<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        acc: &AllocatedNum<F>,
        kv: &AllocatedPtr<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError> {
        let kv_num = kv.hash().clone();
        let element = self.synthesize_map_to_element(&mut cs.namespace(|| "element"), kv_num)?;
        acc.add(&mut cs.namespace(|| "add to acc"), &element)
    }

    fn synthesize_remove_n<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        acc: &AllocatedNum<F>,
        kv: &AllocatedPtr<F>,
        count: &AllocatedNum<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError> {
        let kv_num = kv.hash().clone();
        let element = self.synthesize_map_to_element(&mut cs.namespace(|| "element"), kv_num)?;
        let scaled = element.mul(&mut cs.namespace(|| "scaled"), count)?;
        sub(&mut cs.namespace(|| "add to acc"), acc, &scaled)
    }

    // x is H(k,v) = hash part of (cons k v)
    // 1 / r + x
    fn synthesize_map_to_element<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        x: AllocatedNum<F>,
    ) -> Result<AllocatedNum<F>, SynthesisError> {
        let r = self.r.clone();
        let r_plus_x = r.add(&mut cs.namespace(|| "r+x"), &x)?;

        invert(&mut cs.namespace(|| "invert(r+x)"), &r_plus_x)
    }

    fn count(&self, form: &Ptr) -> usize {
        self.multiset.get(form).unwrap_or(0)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::state::State;
    use bellpepper_core::{test_cs::TestConstraintSystem, Comparable};
    use demo::DemoQuery;
    use expect_test::{expect, Expect};
    use halo2curves::bn256::Fr as F;
    use std::default::Default;

    #[test]
    fn test_query_with_internal_insertion_transcript() {
        test_query_aux(
            true,
            expect!["9430"],
            expect!["9463"],
            expect!["10012"],
            expect!["10049"],
            1,
        );
        test_query_aux(
            true,
            expect!["11174"],
            expect!["11213"],
            expect!["11756"],
            expect!["11799"],
            3,
        );
        test_query_aux(
            true,
            expect!["18216"],
            expect!["18279"],
            expect!["18798"],
            expect!["18865"],
            10,
        )
    }

    #[test]
    fn test_query_without_internal_insertion_transcript() {
        test_query_aux(
            false,
            expect!["7985"],
            expect!["8018"],
            expect!["8567"],
            expect!["8604"],
            1,
        );
        test_query_aux(
            false,
            expect!["9440"],
            expect!["9479"],
            expect!["10022"],
            expect!["10065"],
            3,
        );
        test_query_aux(
            false,
            expect!["15326"],
            expect!["15389"],
            expect!["15908"],
            expect!["15975"],
            10,
        )
    }

    fn test_query_aux(
        transcribe_internal_insertions: bool,
        expected_constraints_simple: Expect,
        expected_aux_simple: Expect,
        expected_constraints_compound: Expect,
        expected_aux_compound: Expect,
        circuit_query_rc: usize,
    ) {
        let s = &Store::<F>::default();
        let mut scope: Scope<DemoQuery<F>, LogMemo<F>> =
            Scope::new(transcribe_internal_insertions, circuit_query_rc);
        let state = State::init_lurk_state();

        let fact_4 = s.read_with_default_state("(factorial . 4)").unwrap();
        let fact_3 = s.read_with_default_state("(factorial . 3)").unwrap();

        let expect_eq = |computed: usize, expected: Expect| {
            expected.assert_eq(&computed.to_string());
        };

        {
            scope.query(s, fact_4);

            for (k, v) in scope.queries.iter() {
                println!("k: {}", k.fmt_to_string(s, &state));
                println!("v: {}", v.fmt_to_string(s, &state));
            }
            // Factorial 4 will memoize calls to:
            // fact(4), fact(3), fact(2), fact(1), and fact(0)
            assert_eq!(5, scope.queries.len());
            assert_eq!(1, scope.toplevel_insertions.len());
            assert_eq!(4, scope.internal_insertions.len());

            scope.finalize_transcript(s);

            let cs = &mut TestConstraintSystem::new();
            let g = &mut GlobalAllocator::default();

            scope.synthesize(cs, g, s).unwrap();

            println!(
                "transcript: {}",
                scope
                    .memoset
                    .transcript
                    .get()
                    .unwrap()
                    .fmt_to_string_simple(s)
            );

            expect_eq(cs.num_constraints(), expected_constraints_simple);
            expect_eq(cs.aux().len(), expected_aux_simple);

            let unsat = cs.which_is_unsatisfied();

            if unsat.is_some() {
                dbg!(unsat);
            }
            assert!(cs.is_satisfied());
        }

        {
            let mut scope: Scope<DemoQuery<F>, LogMemo<F>> =
                Scope::new(transcribe_internal_insertions, circuit_query_rc);
            scope.query(s, fact_4);
            scope.query(s, fact_3);

            // // No new queries.
            assert_eq!(5, scope.queries.len());
            // // One new top-level insertion.
            assert_eq!(2, scope.toplevel_insertions.len());
            // // No new internal insertions.
            assert_eq!(4, scope.internal_insertions.len());

            scope.finalize_transcript(s);

            let cs = &mut TestConstraintSystem::new();
            let g = &mut GlobalAllocator::default();

            scope.synthesize(cs, g, s).unwrap();

            println!(
                "transcript: {}",
                scope
                    .memoset
                    .transcript
                    .get()
                    .unwrap()
                    .fmt_to_string_simple(s)
            );

            expect_eq(cs.num_constraints(), expected_constraints_compound);
            expect_eq(cs.aux().len(), expected_aux_compound);

            let unsat = cs.which_is_unsatisfied();
            if unsat.is_some() {
                dbg!(unsat);
            }
            assert!(cs.is_satisfied());
        }
    }
}
