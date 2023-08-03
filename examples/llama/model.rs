#![allow(clippy::type_complexity)]
use luminal::{
    nn::{activation::RMSNorm, embedding::Embedding},
    op,
    prelude::{movement::TryConcatAlong, *},
};

// Full LLaMa model implementation, heavily based off of https://github.com/coreylowman/llama-dfdx/blob/main/src/modeling.rs

pub struct Mlp<const I: usize, const H: usize> {
    pub gate_proj: GraphTensor<R2<I, H>>,
    pub down_proj: GraphTensor<R2<H, I>>,
    pub up_proj: GraphTensor<R2<I, H>>,
}

impl<const I: usize, const H: usize, B: Dim, S: Dim> Module<GraphTensor<(B, S, Const<H>)>>
    for Mlp<I, H>
{
    type Output = GraphTensor<(B, S, Const<H>)>;

    fn forward(&self, input: GraphTensor<(B, S, Const<H>)>) -> Self::Output {
        let gate = {
            let gate = input.matmul(self.gate_proj.permute());
            gate.sigmoid() * gate
        };
        let up = {
            let up = input.matmul(self.up_proj.permute());
            up * gate
        };
        up.matmul(self.down_proj.permute())
    }
}

pub struct RotaryEmbedding<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize> {
    pub inv_freq: GraphTensor<R1<HEAD_DIM_OVER_2>>,
}

impl<Batch: Dim, NumHeads: Dim, Seq: Dim, const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize>
    Module<(
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        usize,
    )> for RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = (
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    );

    fn forward(
        &self,
        (q, k, offset): (
            GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
            GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
            usize,
        ),
    ) -> Self::Output {
        let (sin, cos) = self.get_sincos(offset, q);
        let sin = sin.expand();
        let cos = cos.expand();
        let q_embed = (Self::rotate_half(q) * sin) + (q * cos);
        let k_embed = (Self::rotate_half(k) * sin) + (k * cos);
        (q_embed, k_embed)
    }
}

impl<const HEAD_DIM: usize, const HEAD_DIM_OVER_2: usize>
    RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>
{
    fn get_sincos<Batch: Dim, NumHeads: Dim, Seq: Dim>(
        &self,
        offset: usize,
        seq_tensor: GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    ) -> (
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
        GraphTensor<(Seq, Const<HEAD_DIM>)>,
    ) {
        let graph = unsafe { self.inv_freq.graph_ref.as_mut().unwrap() };
        let t: GraphTensor<(Seq,)> = GraphTensor::from_id(
            graph
                .add_op(
                    op::Function(Box::new(move |inp, i| {
                        (
                            Some(Tensor {
                                data: Box::new(
                                    (0..inp[0].1.shape.shape()[0])
                                        .map(|i| i as f32)
                                        .collect::<Vec<_>>(),
                                ),
                            }),
                            TensorView {
                                tensor_id: i,
                                shape: ShapeTracker::new(vec![inp[0].1.shape.shape()[0]]),
                            },
                        )
                    })),
                    vec![Seq::const_size()],
                )
                .input(seq_tensor.id)
                .finish(),
            graph,
        ) + offset as f32;
        let freqs = t
            .expand::<(Seq, Const<HEAD_DIM>), _>()
            .matmul(self.inv_freq.expand())
            .realize::<(Seq, usize)>();
        let emb = (freqs, freqs).concat_along(Axis::<1>);
        (emb.sin().realize(), emb.cos().realize())
    }

    fn rotate_half<Batch: Dim, NumHeads: Dim, Seq: Dim>(
        x: GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)>,
    ) -> GraphTensor<(Batch, NumHeads, Seq, Const<HEAD_DIM>)> {
        let x1 = x.slice((.., .., .., ..HEAD_DIM_OVER_2));
        let x2 = x.slice((.., .., .., HEAD_DIM_OVER_2..));
        (-x2, x1).concat_along(Axis::<3>).realize()
    }
}

pub struct Attention<
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub q_proj: GraphTensor<R2<HIDDEN, HIDDEN>>,
    pub k_proj: GraphTensor<R2<HIDDEN, HIDDEN>>,
    pub v_proj: GraphTensor<R2<HIDDEN, HIDDEN>>,
    pub o_proj: GraphTensor<R2<HIDDEN, HIDDEN>>,
    pub rotary_embed: RotaryEmbedding<HEAD_DIM, HEAD_DIM_OVER_2>,
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    >
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
        GraphTensor<(CurSeq, CurSeq)>,
        usize,
    )> for Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;

    fn forward(
        &self,
        (x, attn_mask, past_seq): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
            GraphTensor<(CurSeq, CurSeq)>,
            usize,
        ),
    ) -> Self::Output {
        let q = x
            .matmul(self.q_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                Batch::const_size(),
                CurSeq::const_size(),
                RealDim::Const(NUM_HEADS),
                RealDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let k = x
            .matmul(self.k_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                Batch::const_size(),
                CurSeq::const_size(),
                RealDim::Const(NUM_HEADS),
                RealDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let v = x
            .matmul(self.v_proj.permute())
            .dyn_reshape::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>)>(vec![
                Batch::const_size(),
                CurSeq::const_size(),
                RealDim::Const(NUM_HEADS),
                RealDim::Const(HEAD_DIM),
            ])
            .permute::<_, Axes4<0, 2, 1, 3>>();

        let (q, k) = self.rotary_embed.forward((
            q.realize::<(Batch, Const<NUM_HEADS>, CurSeq, Const<HEAD_DIM>)>(),
            k.realize(),
            past_seq,
        ));
        let inv_head_scale = (HEAD_DIM as f64).sqrt().recip() as f32;
        let w = q.batch_matmul(k.permute()) * inv_head_scale;
        let w = w + attn_mask.expand();
        let w = w.softmax::<3>();

        let o = w.batch_matmul(v);
        let o = o
            .permute::<(Batch, CurSeq, Const<NUM_HEADS>, Const<HEAD_DIM>), _>()
            .reshape::<(Batch, CurSeq, Const<HIDDEN>)>();

        o.matmul(self.o_proj.permute())
    }
}

pub struct DecoderLayer<
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub self_attn: Attention<NUM_HEADS, HIDDEN, HEAD_DIM, HEAD_DIM_OVER_2>,
    pub mlp: Mlp<INTERMEDIATE, HIDDEN>,
    pub input_layer_norm: RMSNorm<HIDDEN>,
    pub post_attention_layer_norm: RMSNorm<HIDDEN>,
}

impl<
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    >
    Module<(
        GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
        GraphTensor<(CurSeq, CurSeq)>,
        usize,
    )> for DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;
    fn forward(
        &self,
        (x, attn_mask, past_seq_size): (
            GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>,
            GraphTensor<(CurSeq, CurSeq)>,
            usize,
        ),
    ) -> Self::Output {
        let x = x + self.self_attn.forward((
            self.input_layer_norm.forward(x),
            attn_mask,
            past_seq_size,
        ));
        x + self.mlp.forward(self.post_attention_layer_norm.forward(x))
    }
}

pub struct Llama<
    const VOCAB: usize,
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub embed_tokens: Embedding<VOCAB, HIDDEN>,
    pub layers: Vec<DecoderLayer<NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>>,
    pub norm: RMSNorm<HIDDEN>,
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    > Module<(GraphTensor<(Batch, CurSeq)>, usize)>
    for Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<HIDDEN>)>;
    fn forward(
        &self,
        (input, past_seq_len): (GraphTensor<(Batch, CurSeq)>, usize),
    ) -> Self::Output {
        let graph = unsafe { self.layers[0].self_attn.k_proj.graph_ref.as_mut().unwrap() };
        let attn_mask: GraphTensor<(CurSeq, CurSeq)> = GraphTensor::from_id(
            graph
                .add_op(
                    op::Function(Box::new(|inp, i| {
                        let seq_len = inp[0].1.shape.shape()[1];
                        let mut data = vec![0.; seq_len];
                        for i in 0..seq_len {
                            for j in 0..i {
                                data[i * seq_len + j] = f32::NEG_INFINITY;
                            }
                        }
                        (
                            Some(Tensor {
                                data: Box::new(data),
                            }),
                            TensorView {
                                tensor_id: i,
                                shape: ShapeTracker::new(vec![
                                    inp[0].1.shape.shape()[1],
                                    inp[0].1.shape.shape()[1],
                                ]),
                            },
                        )
                    })),
                    vec![CurSeq::const_size(), CurSeq::const_size()],
                )
                .input(input.id)
                .finish(),
            graph,
        );

        let mut hidden_states = self.embed_tokens.forward(input);
        for layer_i in &self.layers {
            hidden_states = layer_i.forward((hidden_states, attn_mask, past_seq_len));
        }
        self.norm.forward(hidden_states)
    }
}

pub struct LlamaForCausalLM<
    const VOCAB: usize,
    const NUM_HEADS: usize,
    const HIDDEN: usize,
    const INTERMEDIATE: usize,
    const HEAD_DIM: usize,
    const HEAD_DIM_OVER_2: usize,
> {
    pub llama: Llama<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>,
    pub lm_head: GraphTensor<R2<VOCAB, HIDDEN>>,
}

impl<
        const VOCAB: usize,
        const NUM_HEADS: usize,
        const HIDDEN: usize,
        const INTERMEDIATE: usize,
        const HEAD_DIM: usize,
        const HEAD_DIM_OVER_2: usize,
        Batch: Dim,
        CurSeq: Dim,
    > Module<(GraphTensor<(Batch, CurSeq)>, usize)>
    for LlamaForCausalLM<VOCAB, NUM_HEADS, HIDDEN, INTERMEDIATE, HEAD_DIM, HEAD_DIM_OVER_2>
{
    type Output = GraphTensor<(Batch, CurSeq, Const<VOCAB>)>;
    fn forward(
        &self,
        (input, past_seq_len): (GraphTensor<(Batch, CurSeq)>, usize),
    ) -> Self::Output {
        let hidden_states = self.llama.forward((input, past_seq_len));
        hidden_states.matmul(self.lm_head.permute())
    }
}
