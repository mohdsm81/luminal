use std::{marker::PhantomData, sync::Arc};

use luminal_cudarc::driver::{CudaDevice, CudaFunction, DeviceRepr, LaunchAsync, LaunchConfig};

use luminal::{
    op::*,
    prelude::{petgraph::visit::EdgeRef, *},
};
use rustc_hash::FxHashMap;

use crate::{
    compile_and_load_kernel, constant, get_buffer_from_tensor, get_idx_valid_exps, input_dyn_dims,
    other::CudaARange,
    prim::{CudaAdd, CudaCopyToDevice, CudaLessThan, CudaMul, CudaSumReduce},
    render_dyn_dim_inputs, CudaData, CudaFloat,
};

#[derive(LuminalEqTrue, LuminalPrint, Clone)]
pub struct CudaSub<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaSub<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        out[idx] =
            (({a_valid}) == 0 ? ({type_name})0.0 : inp_a[{a_idx}])
            - (({b_valid}) == 0 ? ({type_name})0.0 : inp_b[{b_idx}]);
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaSub<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();

        let out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);
        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalPrint, Default)]
pub struct SubtractionCompiler<T: CudaFloat>(PhantomData<T>);

impl<T: CudaFloat> Compiler for SubtractionCompiler<T> {
    fn compile<To: ToIdsMut>(&self, graph: &mut Graph, _: To) {
        let dev = CudaDevice::new(0).unwrap();
        let (lhs, rhs) = (node(), node());
        let mul = binary::<CudaMul<T>>(rhs.clone(), constant::<T>(-1.));
        let add = binary::<CudaAdd<T>>(lhs.clone(), mul.clone());
        let mut s = add.clone().search(graph);
        while s.next_match() {
            if s.check_no_delete(&[add.id]) {
                continue;
            }
            let add = s.get(&add);
            let (a, a_edge) = graph
                .graph
                .edges_connecting(s.get(&lhs), add)
                .next()
                .map(|e| (e.source(), e.weight().as_data().unwrap()))
                .unwrap();
            let (b, b_edge) = graph
                .graph
                .edges_connecting(s.get(&rhs), s.get(&mul))
                .next()
                .map(|e| (e.source(), e.weight().as_data().unwrap()))
                .unwrap();
            let b_final_shape = graph
                .graph
                .edges_connecting(s.get(&mul), add)
                .next()
                .unwrap()
                .weight()
                .as_data()
                .unwrap()
                .2;
            if b_final_shape.is_reshaped() {
                continue;
            }
            let sub = graph
                .add_op(CudaSub::<T>::new(
                    a_edge.2,
                    b_edge.2,
                    dev.clone(),
                    &graph.dyn_map,
                ))
                .input(a, a_edge.1, a_edge.2)
                .input(b, b_edge.1, b_edge.2)
                .finish();
            move_outgoing_edge(add, sub, &mut graph.graph);

            graph.graph.remove_node(add);
            s.try_delete();
        }
    }
}

#[derive(LuminalEqTrue, LuminalPrint, Clone)]
pub struct CudaEqual<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaEqual<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        {type_name} a_val = ({a_valid}) == 0 ? ({type_name})0.0 : inp_a[{a_idx}];
        {type_name} b_val = ({b_valid}) == 0 ? ({type_name})0.0 : inp_b[{b_idx}];
        out[idx] = ({type_name})(a_val == b_val);
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaEqual<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();

        let out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);
        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalPrint, Default)]
pub struct EqualCompiler<T: CudaFloat>(PhantomData<T>);

impl<T: CudaFloat> Compiler for EqualCompiler<T> {
    fn compile<To: ToIdsMut>(&self, graph: &mut Graph, _: To) {
        let dev = CudaDevice::new(0).unwrap();
        let one = constant::<T>(1.);
        let (lhs, rhs) = (node(), node());
        let lt1 = binary::<CudaLessThan<T>>(lhs.clone(), rhs.clone());
        let ne = binary::<CudaAdd<T>>(
            lt1.clone(),
            binary::<CudaLessThan<T>>(rhs.clone(), lhs.clone()),
        );
        let eq = binary::<CudaSub<T>>(one, ne);

        let mut s = eq.clone().search(graph);
        while s.next_match() {
            if s.check_no_delete(&[eq.id]) {
                continue;
            }
            let (lhs, rhs) = (s.get(&lhs), s.get(&rhs));
            let eq = s.get(&eq);
            let a_edge = graph
                .graph
                .edges_connecting(lhs, s.get(&lt1))
                .next()
                .unwrap()
                .weight()
                .as_data()
                .unwrap();
            let b_edge = graph
                .graph
                .edges_connecting(rhs, s.get(&lt1))
                .next()
                .unwrap()
                .weight()
                .as_data()
                .unwrap();
            let equals = graph
                .add_op(CudaEqual::<T>::new(
                    a_edge.2,
                    b_edge.2,
                    dev.clone(),
                    &graph.dyn_map,
                ))
                .input(lhs, a_edge.1, a_edge.2)
                .input(rhs, b_edge.1, b_edge.2)
                .finish();
            move_outgoing_edge(eq, equals, &mut graph.graph);

            graph.graph.remove_node(eq);
            s.try_delete();
        }
    }
}

#[derive(LuminalPrint, Clone, LuminalEqFalse)]
pub struct CudaGather<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    pub embed_dim: usize,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaGather<T> {
    pub fn new(device: Arc<CudaDevice>, embed_dim: usize) -> Self {
        let type_name = T::type_name();
        let code = format!("
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *weights, const float *inp, int n_embeddings, int embedding_dim) {{
    int x = blockIdx.x * blockDim.x + threadIdx.x;
    int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x < n_embeddings && y < embedding_dim) {{
        out[x * embedding_dim + y] = weights[(int)inp[x] * embedding_dim + y];
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            embed_dim,
            _phantom: Default::default(),
        }
    }
}

impl<T: CudaFloat> Operator for CudaGather<T> {
    fn process(&mut self, inputs: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        // Inp 1 should be Vec<f32> and inp 2 should be a CudaSlice<T>
        let indexes = inputs[0]
            .0
            .borrowed()
            .data
            .as_any()
            .downcast_ref::<Vec<f32>>()
            .unwrap();
        let weights = get_buffer_from_tensor::<T>(&inputs[1].0);

        let mut indexes_buffer = unsafe { self.device.alloc::<f32>(indexes.len()).unwrap() };
        self.device
            .htod_copy_into(indexes.clone(), &mut indexes_buffer)
            .unwrap();
        let mut out = self
            .device
            .alloc_zeros::<T>(indexes.len() * self.embed_dim)
            .unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig {
                        grid_dim: (
                            indexes.len().div_ceil(16) as u32,
                            self.embed_dim.div_ceil(16) as u32,
                            1,
                        ),
                        block_dim: (16, 16, 1),
                        shared_mem_bytes: 0,
                    },
                    (
                        &mut out,
                        weights,
                        &indexes_buffer,
                        indexes.len(),
                        self.embed_dim,
                    ),
                )
                .unwrap();
        }

        vec![Tensor {
            data: Box::new(CudaData(out)),
        }]
    }
}

#[derive(LuminalPrint, Default)]
pub struct GatherCompiler<T: CudaFloat>(PhantomData<T>);

impl<T: CudaFloat> Compiler for GatherCompiler<T> {
    fn compile<To: ToIdsMut>(&self, graph: &mut Graph, _: To) {
        let dev = CudaDevice::new(0).unwrap();
        let indexes = node();
        let ind_copy = unary::<CudaCopyToDevice<T>>(indexes.clone());
        let equal = binary::<CudaEqual<T>>(op::<CudaARange<T>>(), ind_copy.clone());
        let embeddings = node();
        let mul = binary::<CudaMul<T>>(embeddings.clone(), equal.clone());
        let sum_reduce = unary::<CudaSumReduce<T>>(mul.clone());
        let mut s = sum_reduce.clone().search(graph);
        while s.next_match() {
            if s.check_no_delete(&[sum_reduce.id, embeddings.id, indexes.id]) {
                continue;
            }
            let emb_shape = graph
                .edges_connecting(s.get(&embeddings), s.get(&mul))
                .next()
                .unwrap()
                .weight()
                .as_data()
                .unwrap()
                .2;
            let embed_dim = emb_shape.shape()[2].to_usize().unwrap();
            let index_shape = graph
                .edges_connecting(s.get(&indexes), s.get(&ind_copy))
                .next()
                .unwrap()
                .weight()
                .as_data()
                .unwrap()
                .2;
            let gather = graph
                .add_op(CudaGather::<T>::new(dev.clone(), embed_dim))
                .input(s.get(&indexes), 0, index_shape)
                .input(s.get(&embeddings), 0, emb_shape)
                .finish();
            move_outgoing_edge(s.get(&sum_reduce), gather, graph);
            graph.remove_node(s.get(&sum_reduce));
            s.try_delete();
        }
    }
}
