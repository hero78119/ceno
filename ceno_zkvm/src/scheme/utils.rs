use std::{borrow::Cow, cell::SyncUnsafeCell, ptr, sync::Arc};

use ark_std::iterable::Iterable;
use ff_ext::ExtensionField;
use itertools::Itertools;
use multilinear_extensions::{
    commutative_op_mle_pair_pool,
    mle::{DenseMultilinearExtension, FieldType, IntoMLE},
    op_mle_xa_b_pool, op_mle3_range_pool,
    util::ceil_log2,
    virtual_poly_v2::ArcMultilinearExtension,
};

use ff::Field;

const POOL_CAP: usize = 12;

use rayon::{
    iter::{
        IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
        IntoParallelRefMutIterator, ParallelIterator,
    },
    prelude::ParallelSliceMut,
};

use crate::{
    expression::Expression, scheme::constants::MIN_PAR_SIZE, uint::util::SimpleVecPool,
    utils::next_pow2_instance_padding,
};

/// interleaving multiple mles into mles, and num_limbs indicate number of final limbs vector
/// e.g input [[1,2],[3,4],[5,6],[7,8]], num_limbs=2,log2_per_instance_size=3
/// output [[1,3,5,7,0,0,0,0],[2,4,6,8,0,0,0,0]]
pub(crate) fn interleaving_mles_to_mles<'a, E: ExtensionField>(
    mles: &[ArcMultilinearExtension<E>],
    num_instances: usize,
    num_limbs: usize,
    default: E,
) -> Vec<ArcMultilinearExtension<'a, E>> {
    assert!(num_limbs.is_power_of_two());
    assert!(!mles.is_empty());
    let next_power_of_2 = next_pow2_instance_padding(num_instances);
    assert!(
        mles.iter()
            .all(|mle| mle.evaluations().len() <= next_power_of_2)
    );
    let log2_num_instances = ceil_log2(next_power_of_2);
    let per_fanin_len = (mles[0].evaluations().len() / num_limbs).max(1); // minimal size 1
    let log2_mle_size = ceil_log2(mles.len());
    let log2_num_limbs = ceil_log2(num_limbs);

    (0..num_limbs)
        .into_par_iter()
        .map(|fanin_index| {
            let mut evaluations = vec![
                default;
                1 << (log2_mle_size
                    + log2_num_instances.saturating_sub(log2_num_limbs))
            ];
            let per_instance_size = 1 << log2_mle_size;
            assert!(evaluations.len() >= per_instance_size);
            let start = per_fanin_len * fanin_index;
            if start < num_instances {
                let valid_instances_len = per_fanin_len.min(num_instances - start);
                mles.iter()
                    .enumerate()
                    .for_each(|(i, mle)| match mle.evaluations() {
                        FieldType::Ext(mle) => mle
                            .get(start..(start + valid_instances_len))
                            .unwrap_or(&[])
                            .par_iter()
                            .zip(evaluations.par_chunks_mut(per_instance_size))
                            .with_min_len(MIN_PAR_SIZE)
                            .for_each(|(value, instance)| {
                                assert_eq!(instance.len(), per_instance_size);
                                instance[i] = *value;
                            }),
                        FieldType::Base(mle) => mle
                            .get(start..(start + per_fanin_len))
                            .unwrap_or(&[])
                            .par_iter()
                            .zip(evaluations.par_chunks_mut(per_instance_size))
                            .with_min_len(MIN_PAR_SIZE)
                            .for_each(|(value, instance)| {
                                assert_eq!(instance.len(), per_instance_size);
                                instance[i] = E::from(*value);
                            }),
                        _ => unreachable!(),
                    });
            }
            evaluations.into_mle().into()
        })
        .collect::<Vec<ArcMultilinearExtension<E>>>()
}

macro_rules! tower_mle_4 {
    ($p1:ident, $p2:ident, $q1:ident, $q2:ident, $acc_p:ident, $acc_q:ident, $start_index:ident, $cur_len:ident) => {
        $q1[$start_index..][..$cur_len]
            .par_iter()
            .zip($q2[$start_index..][..$cur_len].par_iter())
            .zip($p1[$start_index..][..$cur_len].par_iter())
            .zip($p2[$start_index..][..$cur_len].par_iter())
            .zip($acc_p.par_iter_mut())
            .zip($acc_q.par_iter_mut())
            .with_min_len(MIN_PAR_SIZE)
            .for_each(|(((((q1, q2), p1), p2), p_eval), q_eval)| {
                *p_eval = *q1 * p2 + *q2 * p1;
                *q_eval = *q1 * q2;
            })
    };
}

/// infer logup witness from last layer
/// return is the ([p1,p2], [q1,q2]) for each layer
pub(crate) fn infer_tower_logup_witness<'a, E: ExtensionField>(
    p_mles: Option<Vec<ArcMultilinearExtension<'a, E>>>,
    q_mles: Vec<ArcMultilinearExtension<'a, E>>,
) -> Vec<Vec<ArcMultilinearExtension<'a, E>>> {
    if cfg!(test) {
        assert_eq!(q_mles.len(), 2);
        assert!(q_mles.iter().map(|q| q.evaluations().len()).all_equal());
    }
    let num_vars = ceil_log2(q_mles[0].evaluations().len());
    let mut wit_layers = (0..num_vars).fold(vec![(p_mles, q_mles)], |mut acc, _| {
        let (p, q): &(
            Option<Vec<ArcMultilinearExtension<E>>>,
            Vec<ArcMultilinearExtension<E>>,
        ) = acc.last().unwrap();
        let (q1, q2) = (&q[0], &q[1]);
        let cur_len = q1.evaluations().len() / 2;
        let (next_p, next_q): (
            Vec<ArcMultilinearExtension<E>>,
            Vec<ArcMultilinearExtension<E>>,
        ) = (0..2)
            .map(|index| {
                let mut p_evals = vec![E::ZERO; cur_len];
                let mut q_evals = vec![E::ZERO; cur_len];
                let start_index = cur_len * index;
                if let Some(p) = p {
                    let (p1, p2) = (&p[0], &p[1]);
                    match (
                        p1.evaluations(),
                        p2.evaluations(),
                        q1.evaluations(),
                        q2.evaluations(),
                    ) {
                        (
                            FieldType::Ext(p1),
                            FieldType::Ext(p2),
                            FieldType::Ext(q1),
                            FieldType::Ext(q2),
                        ) => tower_mle_4!(p1, p2, q1, q2, p_evals, q_evals, start_index, cur_len),
                        (
                            FieldType::Base(p1),
                            FieldType::Base(p2),
                            FieldType::Ext(q1),
                            FieldType::Ext(q2),
                        ) => tower_mle_4!(p1, p2, q1, q2, p_evals, q_evals, start_index, cur_len),
                        _ => unreachable!(),
                    };
                } else {
                    match (q1.evaluations(), q2.evaluations()) {
                        (FieldType::Ext(q1), FieldType::Ext(q2)) => q1[start_index..][..cur_len]
                            .par_iter()
                            .zip(q2[start_index..][..cur_len].par_iter())
                            .zip(p_evals.par_iter_mut())
                            .zip(q_evals.par_iter_mut())
                            .with_min_len(MIN_PAR_SIZE)
                            .for_each(|(((q1, q2), p_res), q_res)| {
                                // 1 / q1 + 1 / q2 = (q1+q2) / q1*q2
                                // p is numerator and q is denominator
                                *p_res = *q1 + q2;
                                *q_res = *q1 * q2;
                            }),
                        _ => unreachable!(),
                    };
                }
                (p_evals.into_mle().into(), q_evals.into_mle().into())
            })
            .unzip(); // vec[vec[p1, p2], vec[q1, q2]]
        acc.push((Some(next_p), next_q));
        acc
    });
    wit_layers.reverse();
    wit_layers
        .into_iter()
        .map(|(p, q)| {
            // input layer p are all 1
            if let Some(p) = p {
                [p, q].concat()
            } else {
                let len = q[0].evaluations().len();
                vec![
                    vec![E::ONE; len].into_mle().into(),
                    vec![E::ONE; len].into_mle().into(),
                ]
                .into_iter()
                .chain(q)
                .collect()
            }
        })
        .collect_vec()
}

/// infer tower witness from last layer
pub(crate) fn infer_tower_product_witness<E: ExtensionField>(
    num_vars: usize,
    last_layer: Vec<ArcMultilinearExtension<'_, E>>,
    num_product_fanin: usize,
) -> Vec<Vec<ArcMultilinearExtension<'_, E>>> {
    assert!(last_layer.len() == num_product_fanin);
    let log2_num_product_fanin = ceil_log2(num_product_fanin);
    let mut wit_layers =
        (0..(num_vars / log2_num_product_fanin) - 1).fold(vec![last_layer], |mut acc, _| {
            let next_layer = acc.last().unwrap();
            let cur_len = next_layer[0].evaluations().len() / num_product_fanin;
            let cur_layer: Vec<ArcMultilinearExtension<E>> = (0..num_product_fanin)
                .map(|index| {
                    let mut evaluations = vec![E::ONE; cur_len];
                    next_layer.iter().for_each(|f| match f.evaluations() {
                        FieldType::Ext(f) => {
                            let start: usize = index * cur_len;
                            f[start..][..cur_len]
                                .par_iter()
                                .zip(evaluations.par_iter_mut())
                                .with_min_len(MIN_PAR_SIZE)
                                .map(|(v, evaluations)| *evaluations *= *v)
                                .collect()
                        }
                        _ => unreachable!("must be extension field"),
                    });
                    evaluations.into_mle().into()
                })
                .collect_vec();
            acc.push(cur_layer);
            acc
        });
    wit_layers.reverse();
    wit_layers
}

fn try_recycle_arcpoly<E: ExtensionField>(
    poly: Cow<ArcMultilinearExtension<'_, E>>,
    pool_e: &mut SimpleVecPool<Vec<E>>,
    pool_b: &mut SimpleVecPool<Vec<E::BaseField>>,
    pool_expected_size_vec: usize,
) {
    fn downcast_arc<E: ExtensionField>(
        arc: ArcMultilinearExtension<'_, E>,
    ) -> DenseMultilinearExtension<E> {
        unsafe {
            // get the raw pointer from the Arc
            assert_eq!(Arc::strong_count(&arc), 1);
            let raw = Arc::into_raw(arc);
            // cast the raw pointer to the desired concrete type
            let typed_ptr = raw as *const DenseMultilinearExtension<E>;
            // manually drop the Arc without dropping the value
            Arc::decrement_strong_count(raw);
            // reconstruct the Arc with the concrete type
            // Move the value out
            ptr::read(typed_ptr)
        }
    }
    let len = poly.evaluations().len();
    if len == pool_expected_size_vec {
        match poly {
            Cow::Borrowed(_) => (),
            Cow::Owned(_) => {
                let poly = downcast_arc(poly.into_owned());

                match poly.evaluations {
                    FieldType::Base(vec) => pool_b.return_to_pool(vec),
                    FieldType::Ext(vec) => pool_e.return_to_pool(vec),
                    _ => unreachable!(),
                };
            }
        };
    }
}

pub(crate) fn wit_infer_by_expr<'a, E: ExtensionField, const N: usize>(
    fixed: &[ArcMultilinearExtension<'a, E>],
    witnesses: &[ArcMultilinearExtension<'a, E>],
    instance: &[ArcMultilinearExtension<'a, E>],
    challenges: &[E; N],
    expr: &Expression<E>,
    n_threads: usize,
) -> ArcMultilinearExtension<'a, E> {
    let len = witnesses[0].evaluations().len();
    let mut pool_e: SimpleVecPool<Vec<_>> = SimpleVecPool::new(POOL_CAP, || {
        (0..len)
            .into_par_iter()
            .with_min_len(MIN_PAR_SIZE)
            .map(|_| E::ZERO)
            .collect::<Vec<E>>()
    });
    let mut pool_b: SimpleVecPool<Vec<_>> = SimpleVecPool::new(POOL_CAP, || {
        (0..len)
            .into_par_iter()
            .with_min_len(MIN_PAR_SIZE)
            .map(|_| E::BaseField::ZERO)
            .collect::<Vec<E::BaseField>>()
    });
    let poly =
        expr.evaluate_with_instance_pool::<Cow<ArcMultilinearExtension<'_, E>>>(
            &|f| Cow::Borrowed(&fixed[f.0]),
            &|witness_id| Cow::Borrowed(&witnesses[witness_id as usize]),
            &|i| Cow::Borrowed(&instance[i.0]),
            &|scalar| {
                let scalar: ArcMultilinearExtension<E> =
                    Arc::new(DenseMultilinearExtension::from_evaluations_vec(0, vec![
                        scalar,
                    ]));
                Cow::Owned(scalar)
            },
            &|challenge_id, pow, scalar, offset| {
                // TODO cache challenge power to be acquired once for each power
                let challenge = challenges[challenge_id as usize];
                let challenge: ArcMultilinearExtension<E> = Arc::new(
                    DenseMultilinearExtension::from_evaluations_ext_vec(0, vec![
                        challenge.pow([pow as u64]) * scalar + offset,
                    ]),
                );
                Cow::Owned(challenge)
            },
            &|cow_a, cow_b, pool_e, pool_b| {
                let (a, b) = (cow_a.as_ref(), cow_b.as_ref());
                let poly =
                    commutative_op_mle_pair_pool!(
                        |a, b, res| {
                            match (a.len(), b.len()) {
                                (1, 1) => {
                                    let poly: ArcMultilinearExtension<_> = Arc::new(
                                        DenseMultilinearExtension::from_evaluation_vec_smart(
                                            0,
                                            vec![a[0] + b[0]],
                                        ),
                                    );
                                    Cow::Owned(poly)
                                }
                                (1, _) => {
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..b.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[0] + b[i];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                                (_, 1) => {
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..a.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[i] + b[0];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                                (_, _) => {
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..a.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[i] + b[i];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                            }
                        },
                        pool_e,
                        pool_b
                    );
                try_recycle_arcpoly(cow_a, pool_e, pool_b, len);
                try_recycle_arcpoly(cow_b, pool_e, pool_b, len);
                poly
            },
            &|cow_a, cow_b, pool_e, pool_b| {
                let (a, b) = (cow_a.as_ref(), cow_b.as_ref());
                let poly =
                    commutative_op_mle_pair_pool!(
                        |a, b, res| {
                            match (a.len(), b.len()) {
                                (1, 1) => {
                                    let poly: ArcMultilinearExtension<_> = Arc::new(
                                        DenseMultilinearExtension::from_evaluation_vec_smart(
                                            0,
                                            vec![a[0] * b[0]],
                                        ),
                                    );
                                    Cow::Owned(poly)
                                }
                                (1, _) => {
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..a.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[0] * b[i];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                                (_, 1) => {
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..a.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[i] * b[0];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                                (_, _) => {
                                    assert_eq!(a.len(), b.len());
                                    // we do the pointwise evaluation multiplication here without involving FFT
                                    // the evaluations outside of range will be checked via sumcheck + identity polynomial
                                    let res = SyncUnsafeCell::new(res);
                                    (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                                        let ptr = (*res.get()).as_mut_ptr();
                                        (0..a.len()).skip(thread_id).step_by(n_threads).for_each(
                                            |i| {
                                                *ptr.add(i) = a[i] * b[i];
                                            },
                                        )
                                    });
                                    Cow::Owned(res.into_inner().into_mle().into())
                                }
                            }
                        },
                        pool_e,
                        pool_b
                    );
                try_recycle_arcpoly(cow_a, pool_e, pool_b, len);
                try_recycle_arcpoly(cow_b, pool_e, pool_b, len);
                poly
            },
            &|cow_x, cow_a, cow_b, pool_e, pool_b| {
                let (x, a, b) = (cow_x.as_ref(), cow_a.as_ref(), cow_b.as_ref());
                let poly = op_mle_xa_b_pool!(
                    |x, a, b, res| {
                        let res = SyncUnsafeCell::new(res);
                        assert_eq!(a.len(), 1);
                        assert_eq!(b.len(), 1);
                        let (a, b) = (a[0], b[0]);
                        (0..n_threads).into_par_iter().for_each(|thread_id| unsafe {
                            let ptr = (*res.get()).as_mut_ptr();
                            (0..x.len())
                                .skip(thread_id)
                                .step_by(n_threads)
                                .for_each(|i| {
                                    *ptr.add(i) = a * x[i] + b;
                                })
                        });
                        Cow::Owned(res.into_inner().into_mle().into())
                    },
                    pool_e,
                    pool_b
                );
                try_recycle_arcpoly(cow_a, pool_e, pool_b, len);
                try_recycle_arcpoly(cow_b, pool_e, pool_b, len);
                try_recycle_arcpoly(cow_x, pool_e, pool_b, len);
                poly
            },
            &mut pool_e,
            &mut pool_b,
        );
    println!("??");
    match poly {
        Cow::Borrowed(poly) => poly.clone(),
        Cow::Owned(_) => poly.into_owned(),
    }
}

pub(crate) fn eval_by_expr<E: ExtensionField>(
    witnesses: &[E],
    challenges: &[E],
    expr: &Expression<E>,
) -> E {
    eval_by_expr_with_fixed(&[], witnesses, challenges, expr)
}

pub(crate) fn eval_by_expr_with_fixed<E: ExtensionField>(
    fixed: &[E],
    witnesses: &[E],
    challenges: &[E],
    expr: &Expression<E>,
) -> E {
    expr.evaluate::<E>(
        &|f| fixed[f.0],
        &|witness_id| witnesses[witness_id as usize],
        &|scalar| scalar.into(),
        &|challenge_id, pow, scalar, offset| {
            // TODO cache challenge power to be acquired once for each power
            let challenge = challenges[challenge_id as usize];
            challenge.pow([pow as u64]) * scalar + offset
        },
        &|a, b| a + b,
        &|a, b| a * b,
        &|x, a, b| a * x + b,
    )
}

pub fn eval_by_expr_with_instance<E: ExtensionField>(
    fixed: &[E],
    witnesses: &[E],
    instance: &[E],
    challenges: &[E],
    expr: &Expression<E>,
) -> E {
    expr.evaluate_with_instance::<E>(
        &|f| fixed[f.0],
        &|witness_id| witnesses[witness_id as usize],
        &|i| instance[i.0],
        &|scalar| scalar.into(),
        &|challenge_id, pow, scalar, offset| {
            // TODO cache challenge power to be acquired once for each power
            let challenge = challenges[challenge_id as usize];
            challenge.pow([pow as u64]) * scalar + offset
        },
        &|a, b| a + b,
        &|a, b| a * b,
        &|x, a, b| a * x + b,
    )
}

#[cfg(test)]
mod tests {
    use ff::Field;
    use goldilocks::{ExtensionField, GoldilocksExt2};
    use itertools::Itertools;
    use multilinear_extensions::{
        commutative_op_mle_pair,
        mle::{FieldType, IntoMLE},
        util::ceil_log2,
        virtual_poly_v2::ArcMultilinearExtension,
    };

    use crate::{
        circuit_builder::{CircuitBuilder, ConstraintSystem},
        expression::{Expression, ToExpr},
        scheme::utils::{
            infer_tower_logup_witness, infer_tower_product_witness, interleaving_mles_to_mles,
            wit_infer_by_expr,
        },
    };

    #[test]
    fn test_infer_tower_witness() {
        type E = GoldilocksExt2;
        let num_product_fanin = 2;
        let last_layer: Vec<ArcMultilinearExtension<E>> = vec![
            vec![E::ONE, E::from(2u64)].into_mle().into(),
            vec![E::from(3u64), E::from(4u64)].into_mle().into(),
        ];
        let num_vars = ceil_log2(last_layer[0].evaluations().len()) + 1;
        let res = infer_tower_product_witness(num_vars, last_layer.clone(), 2);
        let (left, right) = (&res[0][0], &res[0][1]);
        let final_product = commutative_op_mle_pair!(
            |left, right| {
                assert!(left.len() == 1 && right.len() == 1);
                left[0] * right[0]
            },
            |out| E::from_base(&out)
        );
        let expected_final_product: E = last_layer
            .iter()
            .map(|f| match f.evaluations() {
                FieldType::Ext(e) => e.iter().copied().reduce(|a, b| a * b).unwrap(),
                _ => unreachable!(""),
            })
            .product();
        assert_eq!(res.len(), num_vars);
        assert!(
            res.iter()
                .all(|layer_wit| layer_wit.len() == num_product_fanin)
        );
        assert_eq!(final_product, expected_final_product);
    }

    #[test]
    fn test_interleaving_mles_to_mles() {
        type E = GoldilocksExt2;
        let num_product_fanin = 2;
        // [[1, 2], [3, 4], [5, 6], [7, 8]]
        let input_mles: Vec<ArcMultilinearExtension<E>> = vec![
            vec![E::ONE, E::from(2u64)].into_mle().into(),
            vec![E::from(3u64), E::from(4u64)].into_mle().into(),
            vec![E::from(5u64), E::from(6u64)].into_mle().into(),
            vec![E::from(7u64), E::from(8u64)].into_mle().into(),
        ];
        let res = interleaving_mles_to_mles(&input_mles, 2, num_product_fanin, E::ONE);
        // [[1, 3, 5, 7], [2, 4, 6, 8]]
        assert_eq!(res[0].get_ext_field_vec(), vec![
            E::ONE,
            E::from(3u64),
            E::from(5u64),
            E::from(7u64)
        ],);
        assert_eq!(res[1].get_ext_field_vec(), vec![
            E::from(2u64),
            E::from(4u64),
            E::from(6u64),
            E::from(8u64)
        ],);
    }

    #[test]
    fn test_interleaving_mles_to_mles_padding() {
        type E = GoldilocksExt2;
        let num_product_fanin = 2;

        // case 1: test limb level padding
        // [[1,2],[3,4],[5,6]]]
        let input_mles: Vec<ArcMultilinearExtension<E>> = vec![
            vec![E::ONE, E::from(2u64)].into_mle().into(),
            vec![E::from(3u64), E::from(4u64)].into_mle().into(),
            vec![E::from(5u64), E::from(6u64)].into_mle().into(),
        ];
        let res = interleaving_mles_to_mles(&input_mles, 2, num_product_fanin, E::ZERO);
        // [[1, 3, 5, 0], [2, 4, 6, 0]]
        assert_eq!(res[0].get_ext_field_vec(), vec![
            E::ONE,
            E::from(3u64),
            E::from(5u64),
            E::from(0u64)
        ],);
        assert_eq!(res[1].get_ext_field_vec(), vec![
            E::from(2u64),
            E::from(4u64),
            E::from(6u64),
            E::from(0u64)
        ],);

        // case 2: test instance level padding
        // [[1,0],[3,0],[5,0]]]
        let input_mles: Vec<ArcMultilinearExtension<E>> = vec![
            vec![E::ONE, E::from(0u64)].into_mle().into(),
            vec![E::from(3u64), E::from(0u64)].into_mle().into(),
            vec![E::from(5u64), E::from(0u64)].into_mle().into(),
        ];
        let res = interleaving_mles_to_mles(&input_mles, 1, num_product_fanin, E::ONE);
        // [[1, 3, 5, 1], [1, 1, 1, 1]]
        assert_eq!(res[0].get_ext_field_vec(), vec![
            E::ONE,
            E::from(3u64),
            E::from(5u64),
            E::ONE
        ],);
        assert_eq!(res[1].get_ext_field_vec(), vec![E::ONE; 4],);
    }

    #[test]
    fn test_interleaving_mles_to_mles_edgecases() {
        type E = GoldilocksExt2;
        let num_product_fanin = 2;
        // one instance, 2 mles: [[2], [3]]
        let input_mles: Vec<ArcMultilinearExtension<E>> = vec![
            vec![E::from(2u64)].into_mle().into(),
            vec![E::from(3u64)].into_mle().into(),
        ];
        let res = interleaving_mles_to_mles(&input_mles, 1, num_product_fanin, E::ONE);
        // [[2, 3], [1, 1]]
        assert_eq!(res[0].get_ext_field_vec(), vec![
            E::from(2u64),
            E::from(3u64)
        ],);
        assert_eq!(res[1].get_ext_field_vec(), vec![E::ONE, E::ONE],);
    }

    #[test]
    fn test_infer_tower_logup_witness() {
        type E = GoldilocksExt2;
        let num_vars = 2;
        let q: Vec<ArcMultilinearExtension<E>> = vec![
            vec![1, 2, 3, 4]
                .into_iter()
                .map(E::from)
                .collect_vec()
                .into_mle()
                .into(),
            vec![5, 6, 7, 8]
                .into_iter()
                .map(E::from)
                .collect_vec()
                .into_mle()
                .into(),
        ];
        let mut res = infer_tower_logup_witness(None, q);
        assert_eq!(num_vars + 1, res.len());
        // input layer
        let layer = res.pop().unwrap();
        // input layer p
        assert_eq!(
            layer[0].evaluations().clone(),
            FieldType::Ext(vec![1.into(); 4])
        );
        assert_eq!(
            layer[1].evaluations().clone(),
            FieldType::Ext(vec![1.into(); 4])
        );
        // input layer q is none
        assert_eq!(
            layer[2].evaluations().clone(),
            FieldType::Ext(vec![1.into(), 2.into(), 3.into(), 4.into()])
        );
        assert_eq!(
            layer[3].evaluations().clone(),
            FieldType::Ext(vec![5.into(), 6.into(), 7.into(), 8.into()])
        );

        // next layer
        let layer = res.pop().unwrap();
        // next layer p1
        assert_eq!(
            layer[0].evaluations().clone(),
            FieldType::<E>::Ext(vec![
                vec![1 + 5].into_iter().map(E::from).sum::<E>(),
                vec![2 + 6].into_iter().map(E::from).sum::<E>()
            ])
        );
        // next layer p2
        assert_eq!(
            layer[1].evaluations().clone(),
            FieldType::<E>::Ext(vec![
                vec![3 + 7].into_iter().map(E::from).sum::<E>(),
                vec![4 + 8].into_iter().map(E::from).sum::<E>()
            ])
        );
        // next layer q1
        assert_eq!(
            layer[2].evaluations().clone(),
            FieldType::<E>::Ext(vec![
                vec![5].into_iter().map(E::from).sum::<E>(),
                vec![2 * 6].into_iter().map(E::from).sum::<E>()
            ])
        );
        // next layer q2
        assert_eq!(
            layer[3].evaluations().clone(),
            FieldType::<E>::Ext(vec![
                vec![3 * 7].into_iter().map(E::from).sum::<E>(),
                vec![4 * 8].into_iter().map(E::from).sum::<E>()
            ])
        );

        // output layer
        let layer = res.pop().unwrap();
        // p1
        assert_eq!(
            layer[0].evaluations().clone(),
            // p11 * q12 + p12 * q11
            FieldType::<E>::Ext(vec![
                vec![(1 + 5) * (3 * 7) + (3 + 7) * 5]
                    .into_iter()
                    .map(E::from)
                    .sum::<E>(),
            ])
        );
        // p2
        assert_eq!(
            layer[1].evaluations().clone(),
            // p21 * q22 + p22 * q21
            FieldType::<E>::Ext(vec![
                vec![(2 + 6) * (4 * 8) + (4 + 8) * (2 * 6)]
                    .into_iter()
                    .map(E::from)
                    .sum::<E>(),
            ])
        );
        // q1
        assert_eq!(
            layer[2].evaluations().clone(),
            // q12 * q11
            FieldType::<E>::Ext(vec![vec![(3 * 7) * 5].into_iter().map(E::from).sum::<E>(),])
        );
        // q2
        assert_eq!(
            layer[3].evaluations().clone(),
            // q22 * q22
            FieldType::<E>::Ext(vec![
                vec![(4 * 8) * (2 * 6)].into_iter().map(E::from).sum::<E>(),
            ])
        );
    }

    #[test]
    fn test_wit_infer_by_expr_base_field() {
        type E = goldilocks::GoldilocksExt2;
        type B = goldilocks::Goldilocks;
        let mut cs = ConstraintSystem::<E>::new(|| "test");
        let mut cb = CircuitBuilder::new(&mut cs);
        let a = cb.create_witin(|| "a");
        let b = cb.create_witin(|| "b");
        let c = cb.create_witin(|| "c");

        let expr: Expression<E> = a.expr() + b.expr() + a.expr() * b.expr() + (c.expr() * 3 + 2);

        let res = wit_infer_by_expr(
            &[],
            &[
                vec![B::from(1)].into_mle().into(),
                vec![B::from(2)].into_mle().into(),
                vec![B::from(3)].into_mle().into(),
            ],
            &[],
            &[],
            &expr,
            1,
        );
        res.get_base_field_vec();
    }

    #[test]
    fn test_wit_infer_by_expr_ext_field() {
        type E = goldilocks::GoldilocksExt2;
        type B = goldilocks::Goldilocks;
        let mut cs = ConstraintSystem::<E>::new(|| "test");
        let mut cb = CircuitBuilder::new(&mut cs);
        let a = cb.create_witin(|| "a");
        let b = cb.create_witin(|| "b");
        let c = cb.create_witin(|| "c");

        let expr: Expression<E> = a.expr()
            + b.expr()
            + a.expr() * b.expr()
            + (c.expr() * 3 + 2)
            + Expression::Challenge(0, 1, E::ONE, E::ONE);

        let res = wit_infer_by_expr(
            &[],
            &[
                vec![B::from(1)].into_mle().into(),
                vec![B::from(2)].into_mle().into(),
                vec![B::from(3)].into_mle().into(),
            ],
            &[],
            &[E::ONE],
            &expr,
            1,
        );
        res.get_ext_field_vec();
    }
}
