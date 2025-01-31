// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::error::AnyError;
use crate::serialize_op_result;
use crate::Op;
use crate::OpFn;
use crate::OpFnEx;
use crate::OpState;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use std::convert::TryFrom;
use crate::PromiseId;
use crate::OpPayload;
use crate::runtime::JsRuntimeState;
use rusty_v8 as v8;

/// Creates an op that passes data synchronously using JSON.
///
/// The provided function `op_fn` has the following parameters:
/// * `&mut OpState`: the op state, can be used to read/write resources in the runtime from an op.
/// * `V`: the deserializable value that is passed to the Rust function.
/// * `&mut [ZeroCopyBuf]`: raw bytes passed along, usually not needed if the JSON value is used.
///
/// `op_fn` returns a serializable value, which is directly returned to JavaScript.
///
/// When registering an op like this...
/// ```ignore
/// let mut runtime = JsRuntime::new(...);
/// runtime.register_op("hello", deno_core::op_sync(Self::hello_op));
/// runtime.sync_ops_cache();
/// ```
///
/// ...it can be invoked from JS using the provided name, for example:
/// ```js
/// let result = Deno.core.opSync("hello", args);
/// ```
///
/// `runtime.sync_ops_cache()` must be called after registering new ops
/// A more complete example is available in the examples directory.
pub fn op_sync<F, A, B, R>(op_fn: F) -> Box<OpFn>
where
  F: Fn(&mut OpState, A, B) -> Result<R, AnyError> + 'static,
  A: DeserializeOwned,
  B: DeserializeOwned,
  R: Serialize + 'static,
{
  Box::new(move |state, payload| -> Op {
    let result = payload
      .deserialize()
      .and_then(|(a, b)| op_fn(&mut state.borrow_mut(), a, b));
    Op::Sync(serialize_op_result(result, state))
  })
}

/// Creates an op that passes data asynchronously using JSON.
///
/// When this op is dispatched, the runtime doesn't exit while processing it.
/// Use op_async_unref instead if you want to make the runtime exit while processing it.
///
/// The provided function `op_fn` has the following parameters:
/// * `Rc<RefCell<OpState>`: the op state, can be used to read/write resources in the runtime from an op.
/// * `V`: the deserializable value that is passed to the Rust function.
/// * `BufVec`: raw bytes passed along, usually not needed if the JSON value is used.
///
/// `op_fn` returns a future, whose output is a serializable value. This value will be asynchronously
/// returned to JavaScript.
///
/// When registering an op like this...
/// ```ignore
/// let mut runtime = JsRuntime::new(...);
/// runtime.register_op("hello", deno_core::op_async(Self::hello_op));
/// runtime.sync_ops_cache();
/// ```
///
/// ...it can be invoked from JS using the provided name, for example:
/// ```js
/// let future = Deno.core.opAsync("hello", args);
/// ```
///
/// `runtime.sync_ops_cache()` must be called after registering new ops
/// A more complete example is available in the examples directory.
pub fn op_async<F, A, B, R, RV>(op_fn: F) -> Box<OpFn>
where
  F: Fn(Rc<RefCell<OpState>>, A, B) -> R + 'static,
  A: DeserializeOwned,
  B: DeserializeOwned,
  R: Future<Output = Result<RV, AnyError>> + 'static,
  RV: Serialize + 'static,
{
  Box::new(move |state, payload| -> Op {
    let pid = payload.promise_id;
    // Deserialize args, sync error on failure
    let args = match payload.deserialize() {
      Ok(args) => args,
      Err(err) => {
        return Op::Sync(serialize_op_result(Err::<(), AnyError>(err), state))
      }
    };
    let (a, b) = args;

    use crate::futures::FutureExt;
    let fut = op_fn(state.clone(), a, b)
      .map(move |result| (pid, serialize_op_result(result, state)));
    Op::Async(Box::pin(fut))
  })
}

/// Creates an op that passes data asynchronously using JSON.
///
/// When this op is dispatched, the runtime still can exit while processing it.
///
/// The other usages are the same as `op_async`.
pub fn op_async_unref<F, A, B, R, RV>(op_fn: F) -> Box<OpFn>
where
  F: Fn(Rc<RefCell<OpState>>, A, B) -> R + 'static,
  A: DeserializeOwned,
  B: DeserializeOwned,
  R: Future<Output = Result<RV, AnyError>> + 'static,
  RV: Serialize + 'static,
{
  Box::new(move |state, payload| -> Op {
    let pid = payload.promise_id;
    // Deserialize args, sync error on failure
    let args = match payload.deserialize() {
      Ok(args) => args,
      Err(err) => {
        return Op::Sync(serialize_op_result(Err::<(), AnyError>(err), state))
      }
    };
    let (a, b) = args;

    use crate::futures::FutureExt;
    let fut = op_fn(state.clone(), a, b)
      .map(move |result| (pid, serialize_op_result(result, state)));
    Op::AsyncUnref(Box::pin(fut))
  })
}

pub fn op_json2raw<F>(op_fn: F) -> Box<OpFnEx>
where
F: Fn(Rc<RefCell<OpState>>, OpPayload) -> Op + 'static,
{
  Box::new(move |mut state: std::cell::RefMut<JsRuntimeState>, op_state: Rc<RefCell<OpState>>, scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, rv: &mut v8::ReturnValue| {
    let state = &mut state;
    // PromiseId
    let arg1 = args.get(1);
    let promise_id = if arg1.is_null_or_undefined() {
      Ok(0) // Accept null or undefined as 0
    } else {
      // Otherwise expect int
      v8::Local::<v8::Integer>::try_from(arg1)
        .map(|l| l.value() as PromiseId)
        .map_err(AnyError::from)
    };
    // Fail if promise id invalid (not null/undefined or int)
    let promise_id: PromiseId = match promise_id {
      Ok(promise_id) => promise_id,
      Err(err) => {
        crate::bindings::throw_type_error(scope, format!("invalid promise id: {}", err));
        return;
      }
    };

    // Deserializable args (may be structured args or ZeroCopyBuf)
    let a = args.get(2);
    let b = args.get(3);

    let payload = OpPayload {
      scope,
      a,
      b,
      promise_id,
    };

    let op = op_fn(op_state, payload);
    match op {
      Op::Sync(result) => {
        rv.set(result.to_v8(scope).unwrap());
      }
      Op::Async(fut) => {
        state.pending_ops.push(fut);
        //state.have_unpolled_ops = true;
        state.waker.wake();
      }
      Op::AsyncUnref(fut) => {
        state.pending_unref_ops.push(fut);
        //state.have_unpolled_ops = true;
        state.waker.wake();
      }
      Op::NotFound => {
        crate::bindings::throw_type_error(scope, format!("Unknown op"));
      }
    };
  })
}

#[macro_export]
macro_rules! get_args {
    ($scope: expr, $type: ty, $args: expr, $index: expr) => {
        {
            match v8::Local::<$type>::try_from($args.get($index)).map_err(AnyError::from) {
                Ok(v) => v,
                Err(err) => {
                    let msg = format!("invalid argument at position {}: {}", $index, err);
                    let msg = v8::String::new($scope, &msg).unwrap();
                    let exc = v8::Exception::type_error($scope, msg);
                    $scope.throw_exception(exc);
                    return;
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn op_async_stack_trace() {
    let mut runtime = crate::JsRuntime::new(Default::default());

    async fn op_throw(
      _state: Rc<RefCell<OpState>>,
      msg: Option<String>,
      _: (),
    ) -> Result<(), AnyError> {
      assert_eq!(msg.unwrap(), "hello");
      Err(crate::error::generic_error("foo"))
    }

    runtime.register_op("op_throw", op_async(op_throw));
    runtime.sync_ops_cache();
    runtime
      .execute_script(
        "<init>",
        r#"
    async function f1() {
      await Deno.core.opAsync('op_throw', 'hello');
    }

    async function f2() {
      await f1();
    }

    f2();
    "#,
      )
      .unwrap();
    let e = runtime.run_event_loop(false).await.unwrap_err().to_string();
    println!("{}", e);
    assert!(e.contains("Error: foo"));
    assert!(e.contains("at async f1 (<init>:"));
    assert!(e.contains("at async f2 (<init>:"));
  }
}
