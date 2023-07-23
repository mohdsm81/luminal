use rand::{thread_rng, Rng};

use crate::prelude::*;

/// A simple linear layer
pub struct Linear<const A: usize, const B: usize> {
    weight: GraphTensor<R2<A, B>>,
}

impl<const A: usize, const B: usize> InitModule for Linear<A, B> {
    fn initialize(cx: &mut Graph) -> Self {
        let s = Self {
            weight: cx.new_tensor(),
        };
        // Init weight has uniforn(-1, 1)
        let mut rng = thread_rng();
        s.weight
            .set((0..(A * B)).map(|_| rng.gen_range(-1_f32..1_f32)).collect());
        s
    }
}

// Single
impl<const A: usize, const B: usize> Module<GraphTensor<R1<A>>> for Linear<A, B> {
    type Output = GraphTensor<R1<B>>;

    fn forward(&self, input: GraphTensor<R1<A>>) -> Self::Output {
        input.matmul(self.weight)
    }
}

// Batched
impl<const A: usize, const B: usize, const C: usize> Module<GraphTensor<R2<C, A>>>
    for Linear<A, B>
{
    type Output = GraphTensor<R2<C, B>>;

    fn forward(&self, input: GraphTensor<R2<C, A>>) -> Self::Output {
        input.matmul(self.weight)
    }
}

impl<const A: usize, const B: usize> LoadModule for Linear<A, B> {
    fn load(&mut self, state_dict: &mut StateDict) {
        self.weight.set(state_dict.data.remove("weight").unwrap().0)
    }
}

#[cfg(test)]
mod tests {
    use super::Linear;
    use crate::{prelude::*, tests::assert_close};
    #[test]
    fn test_linear() {
        let mut cx = Graph::new();
        let batch = cx.new_tensor::<R2<2, 3>>();
        let a = cx.new_tensor::<R1<3>>();

        let model: Linear<3, 4> = Linear::initialize(&mut cx);
        let b = model.forward(a);
        let batch_out = model.forward(batch);

        b.mark();
        a.mark();
        batch_out.mark();
        a.set(vec![1.0, 2.0, 3.0]);
        batch.set(vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0]);
        cx.execute();

        let unoptimized_b = b.retrieve().unwrap();
        let unoptimized_batch_out = batch_out.retrieve().unwrap();

        cx.optimize(GeneralOpt::default());
        cx.execute();

        assert_close(&unoptimized_b, &b.retrieve().unwrap());
        assert_close(&unoptimized_batch_out, &batch_out.retrieve().unwrap());
    }
}