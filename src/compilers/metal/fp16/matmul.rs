use std::sync::Arc;

use half::f16;
use petgraph::stable_graph::NodeIndex;

use crate::{
    compilers::metal::*,
    op::{InputTensor, Operator},
    prelude::*,
};

use metal_rs::{objc::rc::autoreleasepool, *};

/// Multiplies a MxK matrix with a KxN matrix, resulting in a MxN matrix
#[derive(LuminalEq, LuminalPrint, Clone)]
pub struct MetalMatmul2D(ComputePipelineState, CommandQueue, Device);

impl MetalMatmul2D {
    fn compile(dev: &Device, a_row_major: bool, b_row_major: bool) -> ComputePipelineState {
        compile_function(
            "kernel_matmul_2d",
            &format!(
                "
#include <metal_stdlib>
using namespace metal;

kernel void kernel_matmul_2d(
    device half *A [[buffer(0)]],
    device half *B [[buffer(1)]],
    device half *C [[buffer(2)]],
    device uint& M [[buffer(3)]],
    device uint& K [[buffer(4)]],
    device uint& N [[buffer(5)]],
    device uint& A_major [[buffer(6)]],
    device uint& B_major [[buffer(7)]],
    uint tid [[thread_position_in_grid]]
) {{
    uint row = tid / N;
    uint column = tid % N;

    if(row < M && column < N) {{
        float value = 0.0f;
        for(int i = 0; i < K; ++i) {{
            uint A_index = {};
            uint B_index = {};
            value = fast::fma((float)A[A_index], (float)B[B_index], value);
        }}
        C[row * N + column] = (half)value;
    }}
}}",
                if a_row_major {
                    "row * K + i"
                } else {
                    "i * M + row"
                },
                if b_row_major {
                    "i * N + column"
                } else {
                    "column * K + i"
                }
            ),
            dev,
        )
    }
}

impl MetalKernelForward for MetalMatmul2D {
    fn metal_forward(
        &self,
        inputs: &[(&Buffer, ShapeTracker)],
        dev: &Device,
        command_buffer: &CommandBufferRef,
    ) -> Vec<Buffer> {
        let (a_shape, b_shape) = (inputs[0].1.shape(), inputs[1].1.shape());
        let (a_strides, b_strides) = (inputs[0].1.strides(), inputs[1].1.strides());
        let (a_row_major, b_row_major) = (a_strides[0] > a_strides[1], b_strides[0] > b_strides[1]);
        let (m, k, n) = (
            a_shape[0].to_usize().unwrap(),
            a_shape[1].to_usize().unwrap(),
            b_shape[1].to_usize().unwrap(),
        );

        let out = dev.new_buffer(
            (m * n * std::mem::size_of::<f16>()) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let encoder =
            command_buffer.compute_command_encoder_with_descriptor(ComputePassDescriptor::new());
        encoder.set_compute_pipeline_state(&self.0);

        // Set inputs
        encoder.set_buffer(0, Some(inputs[0].0), 0);
        encoder.set_buffer(1, Some(inputs[1].0), 0);
        encoder.set_buffer(2, Some(&out), 0);
        encoder.set_int(3, m as u32);
        encoder.set_int(4, k as u32);
        encoder.set_int(5, n as u32);
        encoder.set_int(6, a_row_major as u32);
        encoder.set_int(7, b_row_major as u32);
        // encoder.set_threadgroup_memory_length(0, 16 * 16 * 4 * std::mem::size_of::<f32>() as u64);

        // Execute
        encoder.dispatch_threads(
            MTLSize {
                width: m as u64,
                height: n as u64,
                depth: 1,
            },
            MTLSize {
                width: 16,
                height: 16,
                depth: 1,
            },
        );
        encoder.end_encoding();

        vec![out]
    }
}

impl Operator for MetalMatmul2D {
    fn process(&self, inp: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        autoreleasepool(|| {
            let a = inp[0]
                .0
                .borrowed()
                .data
                .as_any()
                .downcast_ref::<Buffer>()
                .unwrap();
            let b = inp[1]
                .0
                .borrowed()
                .data
                .as_any()
                .downcast_ref::<Buffer>()
                .unwrap();

            // Setup command queue / command buffer / encoder
            let command_buffer = self.1.new_command_buffer();

            let out = self
                .metal_forward(&[(a, inp[0].1), (b, inp[1].1)], &self.2, command_buffer)
                .pop()
                .unwrap();

            command_buffer.commit();
            command_buffer.wait_until_completed();

            vec![Tensor {
                data: Box::new(out),
            }]
        })
    }

    fn custom(&self, key: &str) -> Option<Box<dyn Any>> {
        if key == "metal" {
            return Some(Box::new(MetalKernelWrapper(Arc::new(Box::new(
                self.clone(),
            )))));
        }
        None
    }
}

/// Multiplies a BxMxK matrix with a BxKxN matrix, resulting in a BxMxN matrix
#[derive(LuminalEq, LuminalPrint, Clone)]
pub struct MetalBatchMatmul2D(ComputePipelineState, CommandQueue, Device);

impl MetalBatchMatmul2D {
    fn compile(dev: &Device, a_row_major: bool, b_row_major: bool) -> ComputePipelineState {
        compile_function(
            "kernel_batch_matmul_2d",
            &format!(
                "
#include <metal_stdlib>
using namespace metal;

kernel void kernel_batch_matmul_2d(
    device half *A [[buffer(0)]],
    device half *B [[buffer(1)]],
    device half *C [[buffer(2)]],
    device uint& Batch [[buffer(3)]],
    device uint& M [[buffer(4)]],
    device uint& K [[buffer(5)]],
    device uint& N [[buffer(6)]],
    device uint& A_major [[buffer(7)]],
    device uint& B_major [[buffer(8)]],
    device uint& A_batch_stride [[buffer(9)]],
    uint3 global_pos [[thread_position_in_grid]]
) {{
    uint batch = global_pos.z;
    uint row = global_pos.x;
    uint column = global_pos.y;

    if(batch < Batch && row < M && column < N) {{
        float value = 0.0f;
        for(uint i = 0; i < K; ++i) {{
            uint A_index = batch * A_batch_stride + {};
            uint B_index = {};
            value = fast::fma((float)A[A_index], (float)B[B_index], value);
        }}
        C[batch * M * N + row * N + column] = (half)value;
    }}
}}",
                if a_row_major {
                    "row * K + i"
                } else {
                    "i * M + row"
                },
                if b_row_major {
                    "i * N + column"
                } else {
                    "column * K + i"
                }
            ),
            dev,
        )
    }
}

impl MetalKernelForward for MetalBatchMatmul2D {
    fn metal_forward(
        &self,
        inputs: &[(&Buffer, ShapeTracker)],
        dev: &Device,
        command_buffer: &CommandBufferRef,
    ) -> Vec<Buffer> {
        let (a_shape, b_shape) = (inputs[0].1.shape(), inputs[1].1.shape());
        let (a_strides, b_strides) = (inputs[0].1.strides(), inputs[1].1.strides());
        let (a_row_major, b_row_major) = (a_strides[1] > a_strides[2], b_strides[0] > b_strides[1]);
        let (batch_size, m, k, n) = (
            a_shape[0].to_usize().unwrap(),
            a_shape[1].to_usize().unwrap(),
            a_shape[2].to_usize().unwrap(),
            b_shape[1].to_usize().unwrap(),
        );

        let out = dev.new_buffer(
            (batch_size * m * n * std::mem::size_of::<f16>()) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let encoder =
            command_buffer.compute_command_encoder_with_descriptor(ComputePassDescriptor::new());
        encoder.set_compute_pipeline_state(&self.0);

        // Set inputs
        encoder.set_buffer(0, Some(inputs[0].0), 0);
        encoder.set_buffer(1, Some(inputs[1].0), 0);
        encoder.set_buffer(2, Some(&out), 0);
        encoder.set_int(3, batch_size as u32);
        encoder.set_int(4, m as u32);
        encoder.set_int(5, k as u32);
        encoder.set_int(6, n as u32);
        encoder.set_int(7, a_row_major as u32);
        encoder.set_int(8, b_row_major as u32);
        encoder.set_int(9, a_strides[0] as u32);

        // Execute
        encoder.dispatch_threads(
            MTLSize {
                width: m as u64,
                height: n as u64,
                depth: batch_size as u64,
            },
            MTLSize {
                width: 16,
                height: 16,
                depth: 1,
            },
        );
        encoder.end_encoding();

        vec![out]
    }
}

impl Operator for MetalBatchMatmul2D {
    fn process(&self, inp: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        autoreleasepool(|| {
            let a = inp[0]
                .0
                .borrowed()
                .data
                .as_any()
                .downcast_ref::<Buffer>()
                .unwrap();
            let b = inp[1]
                .0
                .borrowed()
                .data
                .as_any()
                .downcast_ref::<Buffer>()
                .unwrap();

            // Setup command queue / command buffer / encoder
            let command_buffer = self.1.new_command_buffer();

            let out = self
                .metal_forward(&[(a, inp[0].1), (b, inp[1].1)], &self.2, command_buffer)
                .pop()
                .unwrap();

            command_buffer.commit();
            command_buffer.wait_until_completed();

            vec![Tensor {
                data: Box::new(out),
            }]
        })
    }

    fn custom(&self, key: &str) -> Option<Box<dyn Any>> {
        if key == "metal" {
            return Some(Box::new(MetalKernelWrapper(Arc::new(Box::new(
                self.clone(),
            )))));
        }
        None
    }
}

#[derive(Default)]
pub struct MetalMatMulCompiler;

impl Compiler for MetalMatMulCompiler {
    fn compile(&self, graph: &mut Graph) {
        let dev = Device::system_default().unwrap();
        let queue = dev.new_command_queue();
        // Look for the matmul pattern
        // Mul ([A, C(fake), B] | [A(fake), C, B]) -> SumReduce(2) -> [A, C]
        // Actually starts at [A,B] | [B, C]
        let (mut sum_reduce, mut mul) = (NodeIndex::default(), NodeIndex::default());
        let s = SelectEdge::new(
            SelectOp::new()
                .ty::<MetalMul<f16>>()
                .shapes(vec![
                    vec![Dim::Unknown('A'), Dim::Unknown('C'), Dim::Unknown('B')],
                    vec![Dim::Unknown('A'), Dim::Unknown('C'), Dim::Unknown('B')],
                ])
                .fakes(vec![vec![false, true, false], vec![true, false, false]])
                .ptr(&mut mul),
            SelectOp::new()
                .ty::<MetalSumReduce<f16>>()
                .check(|o, _| {
                    if let Some(o) = o.as_any().downcast_ref::<MetalSumReduce<f16>>() {
                        o.3 == 2
                    } else {
                        false
                    }
                })
                .ptr(&mut sum_reduce),
        );

        let mut matmul = None;
        for _ in s.search(graph) {
            if graph.no_delete.contains(&mul) {
                // The intermediate mul can't be deleted
                continue;
            }
            // Insert MatMul2D op
            let mut srcs = graph.get_sources(mul);
            // Undo expansions and permute
            srcs[0].2.remove_dim(1);
            srcs[1].2.remove_dim(0);
            srcs[1].2.permute(&[1, 0]);
            if matmul.is_none() {
                matmul = Some(MetalMatmul2D::compile(
                    &dev,
                    srcs[0].2.indexes[0] < srcs[0].2.indexes[1],
                    srcs[1].2.indexes[0] < srcs[1].2.indexes[1],
                ));
            }
            let new_op = graph
                .add_op(MetalMatmul2D(
                    matmul.clone().unwrap(),
                    queue.clone(),
                    dev.clone(),
                ))
                .input(srcs[0].0, 0, srcs[0].2)
                .input(srcs[1].0, 0, srcs[1].2)
                .finish();

            // Create edges to dests
            move_outgoing_edge(sum_reduce, new_op, &mut graph.graph);
            move_references(
                &mut graph.id_remap,
                &mut graph.no_delete,
                &mut graph.to_retrieve,
                sum_reduce,
                new_op,
            );

            // Remove the old ops
            graph.graph.remove_node(mul);
            graph.graph.remove_node(sum_reduce);
        }

        // Look for the batch matmul pattern
        // Mul ([A, C(fake), B] | [A(fake), C, B]) -> SumReduce(2) -> [A, C]
        // Actually starts at [A,B] | [B, C]
        let (mut sum_reduce, mut mul) = (NodeIndex::default(), NodeIndex::default());
        let s = SelectEdge::new(
            SelectOp::new()
                .ty::<MetalMul<f16>>()
                .shapes(vec![
                    vec![
                        Dim::Unknown('D'),
                        Dim::Unknown('A'),
                        Dim::Unknown('C'),
                        Dim::Unknown('B'),
                    ],
                    vec![
                        Dim::Unknown('D'),
                        Dim::Unknown('A'),
                        Dim::Unknown('C'),
                        Dim::Unknown('B'),
                    ],
                ])
                .fakes(vec![
                    vec![false, false, true, false],
                    vec![true, true, false, false],
                ])
                .ptr(&mut mul),
            SelectOp::new()
                .ty::<MetalSumReduce<f16>>()
                .check(|o, _| {
                    if let Some(o) = o.as_any().downcast_ref::<MetalSumReduce<f16>>() {
                        o.3 == 3
                    } else {
                        false
                    }
                })
                .ptr(&mut sum_reduce),
        );
        let mut batched_matmul = None;
        for _ in s.search(graph) {
            if graph.no_delete.contains(&mul) {
                // The intermediate mul can't be deleted
                continue;
            }
            // Insert BatchMatMul2D op
            let mut srcs = graph.get_sources(mul);
            // Undo expansions and permute
            srcs[0].2.remove_dim(2);
            srcs[1].2.remove_dim(1);
            srcs[1].2.remove_dim(0);
            srcs[1].2.permute(&[1, 0]);
            if batched_matmul.is_none() {
                batched_matmul = Some(MetalBatchMatmul2D::compile(
                    &dev,
                    srcs[0].2.indexes[1] < srcs[0].2.indexes[2],
                    srcs[1].2.indexes[0] < srcs[1].2.indexes[1],
                ));
            }
            let new_op = graph
                .add_op(MetalBatchMatmul2D(
                    batched_matmul.clone().unwrap(),
                    queue.clone(),
                    dev.clone(),
                ))
                .input(srcs[0].0, 0, srcs[0].2)
                .input(srcs[1].0, 0, srcs[1].2)
                .finish();

            // Create edges to dests
            move_outgoing_edge(sum_reduce, new_op, &mut graph.graph);
            move_references(
                &mut graph.id_remap,
                &mut graph.no_delete,
                &mut graph.to_retrieve,
                sum_reduce,
                new_op,
            );

            // Remove the old ops
            graph.graph.remove_node(mul);
            graph.graph.remove_node(sum_reduce);
        }
    }
}
