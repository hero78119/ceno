use ff_ext::ExtensionField;
use multilinear_extensions::mle::DenseMultilinearExtension;
use whir::poly_utils::coeffs::CoefficientList;

use crate::util::arithmetic::interpolate_field_type_over_boolean_hypercube;

use super::ff_base::BaseFieldWrapper;

pub fn poly2whir<E: ExtensionField>(
    poly: &DenseMultilinearExtension<E>,
) -> CoefficientList<BaseFieldWrapper<E>> {
    let mut poly = poly.clone();
    interpolate_field_type_over_boolean_hypercube(&mut poly.evaluations);
    match &poly.evaluations {
        multilinear_extensions::mle::FieldType::Ext(_coeffs) => {
            panic!("WHIR only supports committing to base field polys now")
        }
        multilinear_extensions::mle::FieldType::Base(coeffs) => {
            CoefficientList::new(coeffs.iter().map(|x| BaseFieldWrapper(*x)).collect())
        }
        _ => unreachable!(),
    }
}
