use std::{
    ffi::c_void,
    ptr, slice,
    sync::{Arc, Mutex},
    time::Duration,
};

use open62541_sys::{
    UA_Client, UA_Client_disconnect, UA_Client_run_iterate, UA_Client_sendAsyncRequest, UA_UInt32,
    UA_STATUSCODE_BADDISCONNECT,
};
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{self, Instant, MissedTickBehavior},
};

use crate::{
    ua, AsyncSubscription, CallbackOnce, ClientBuilder, DataType, Error, ServiceRequest,
    ServiceResponse,
};

/// Connected OPC UA client (with asynchronous API).
pub struct AsyncClient {
    client: Arc<Mutex<ua::Client>>,
    background_handle: JoinHandle<()>,
}

impl AsyncClient {
    /// Creates client connected to endpoint.
    ///
    /// If you need more control over the initialization, use [`ClientBuilder`] instead, and turn it
    /// into [`Client`](crate::Client) by calling [`connect()`](ClientBuilder::connect), followed by
    /// [`into_async()`](crate::Client::into_async) to get the asynchronous API.
    ///
    /// # Errors
    ///
    /// See [`ClientBuilder::connect()`] and [`Client::into_async()`](crate::Client::into_async).
    ///
    /// # Panics
    ///
    /// See [`ClientBuilder::connect()`].
    pub fn new(endpoint_url: &str, cycle_time: Duration) -> Result<Self, Error> {
        Ok(ClientBuilder::default()
            .connect(endpoint_url)?
            .into_async(cycle_time))
    }

    pub(crate) fn from_sync(client: ua::Client, cycle_time: Duration) -> Self {
        let client = Arc::new(Mutex::new(client));

        let background_task = background_task(Arc::clone(&client), cycle_time);
        // Run the event loop concurrently. This may be a different thread when using tokio with
        // `rt-multi-thread`.
        let background_handle = tokio::spawn(background_task);

        Self {
            client,
            background_handle,
        }
    }

    /// Gets current channel and session state, and connect status.
    ///
    /// # Errors
    ///
    /// This only fails when the client has an internal error.
    pub fn state(&self) -> Result<ua::ClientState, Error> {
        let Ok(mut client) = self.client.lock() else {
            return Err(Error::internal("should be able to lock client"));
        };

        Ok(client.state())
    }

    /// Reads node attribute.
    ///
    /// To read only the value attribute, you can also use [`read_value()`].
    ///
    /// # Errors
    ///
    /// This fails when the node does not exist or the attribute cannot be read.
    ///
    /// [`read_value()`]: Self::read_value
    #[allow(clippy::missing_panics_doc)]
    pub async fn read_attribute(
        &self,
        node_id: &ua::NodeId,
        attribute_id: &ua::AttributeId,
    ) -> Result<ua::DataValue, Error> {
        let mut values = self
            .read_attributes_array(node_id, slice::from_ref(attribute_id))
            .await?;

        // ERROR: We give a slice with one item to `read_attributes()` and expect
        // a single result value.
        debug_assert_eq!(values.len(), 1);
        let value = values
            .drain_all()
            .next()
            .expect("should contain exactly one attribute");
        Ok(value)
    }

    /// Reads node value.
    ///
    /// To read other attributes, see [`read_attribute()`] and [`read_attributes()`].
    ///
    /// # Errors
    ///
    /// This fails when the node does not exist or its value attribute cannot be read.
    ///
    /// [`read_attribute()`]: Self::read_attribute
    /// [`read_attributes()`]: Self::read_attributes
    pub async fn read_value(&self, node_id: &ua::NodeId) -> Result<ua::DataValue, Error> {
        self.read_attribute(node_id, &ua::AttributeId::VALUE).await
    }

    /// Reads several node attributes.
    ///
    /// The size and order of the result list matches the size and order of the given attribute ID
    /// list.
    ///
    /// To read only a single attribute, you can also use [`read_attributes()`].
    ///
    /// # Errors
    ///
    /// This fails when the node does not exist or one of the attributes cannot be read.
    ///
    /// [`read_attributes()`]: Self::read_attributes
    pub async fn read_attributes(
        &self,
        node_id: &ua::NodeId,
        attribute_ids: &[ua::AttributeId],
    ) -> Result<Vec<ua::DataValue>, Error> {
        self.read_attributes_array(node_id, attribute_ids)
            .await
            .map(ua::Array::into_vec)
    }

    pub(crate) async fn read_attributes_array(
        &self,
        node_id: &ua::NodeId,
        attribute_ids: &[ua::AttributeId],
    ) -> Result<ua::Array<ua::DataValue>, Error> {
        let nodes_to_read: Vec<_> = attribute_ids
            .iter()
            .map(|attribute_id| {
                ua::ReadValueId::init()
                    .with_node_id(node_id)
                    .with_attribute_id(attribute_id)
            })
            .collect();

        let request = ua::ReadRequest::init().with_nodes_to_read(&nodes_to_read);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("read should return results"));
        };

        // The OPC UA specification state that the resulting list has the same number of elements as
        // the request list. If not, we would not be able to match elements in the two lists anyway.
        debug_assert_eq!(results.len(), attribute_ids.len());

        Ok(results)
    }

    /// Writes node value.
    ///
    /// # Errors
    ///
    /// This fails when the node does not exist or its value attribute cannot be written.
    pub async fn write_value(
        &self,
        node_id: &ua::NodeId,
        value: &ua::DataValue,
    ) -> Result<(), Error> {
        let attribute_id = ua::AttributeId::VALUE;

        let request = ua::WriteRequest::init().with_nodes_to_write(&[ua::WriteValue::init()
            .with_node_id(node_id)
            .with_attribute_id(&attribute_id)
            .with_value(value)]);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("write should return results"));
        };

        let Some(result) = results.as_slice().first() else {
            return Err(Error::internal("write should return a result"));
        };

        Error::verify_good(result)?;

        Ok(())
    }

    /// Calls specific method node at object node.
    ///
    /// # Errors
    ///
    /// This fails when the object or method node does not exist, the method cannot be called, or
    /// the input arguments are unexpected.
    pub async fn call_method(
        &self,
        object_id: &ua::NodeId,
        method_id: &ua::NodeId,
        input_arguments: &[ua::Variant],
    ) -> Result<Option<Vec<ua::Variant>>, Error> {
        let request =
            ua::CallRequest::init().with_methods_to_call(&[ua::CallMethodRequest::init()
                .with_object_id(object_id)
                .with_method_id(method_id)
                .with_input_arguments(input_arguments)]);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("call should return results"));
        };

        let Some(result) = results.as_slice().first() else {
            return Err(Error::internal("call should return a result"));
        };

        Error::verify_good(&result.status_code())?;

        let Some(output_arguments) = result.output_arguments() else {
            return Ok(None);
        };

        Ok(Some(output_arguments.into_vec()))
    }

    /// Browses specific node.
    ///
    /// # Errors
    ///
    /// This fails when the node does not exist or it cannot be browsed.
    pub async fn browse(
        &self,
        node_id: &ua::NodeId,
    ) -> Result<(Vec<ua::ReferenceDescription>, Option<ua::ContinuationPoint>), Error> {
        let request = ua::BrowseRequest::init()
            .with_nodes_to_browse(&[ua::BrowseDescription::default().with_node_id(node_id)]);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("browse should return results"));
        };

        let Some(result) = results.as_slice().first() else {
            return Err(Error::internal("browse should return a result"));
        };

        let Some(references) = result.references() else {
            return Err(Error::internal("browse should return references"));
        };

        Ok((references.into_vec(), result.continuation_point()))
    }

    /// Browses several nodes at once.
    ///
    /// This issues only a single request to the OPC UA server (and should be preferred over several
    /// individual requests with [`browse()`] when browsing multiple nodes).
    ///
    /// The size and order of the result list matches the size and order of the given node ID list.
    ///
    /// # Errors
    ///
    /// This fails when any of the given nodes does not exist or cannot be browsed.
    ///
    /// [`browse()`]: Self::browse
    pub async fn browse_many(
        &self,
        node_ids: &[ua::NodeId],
    ) -> Result<Vec<Option<(Vec<ua::ReferenceDescription>, Option<ua::ContinuationPoint>)>>, Error>
    {
        let nodes_to_browse: Vec<_> = node_ids
            .iter()
            .map(|node_id| ua::BrowseDescription::default().with_node_id(node_id))
            .collect();

        let request = ua::BrowseRequest::init().with_nodes_to_browse(&nodes_to_browse);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("browse should return results"));
        };

        let results: Vec<_> = results
            .iter()
            .map(|result| {
                result
                    .references()
                    .map(|references| (references.into_vec(), result.continuation_point()))
            })
            .collect();

        // The OPC UA specification state that the resulting list has the same number of elements as
        // the request list. If not, we would not be able to match elements in the two lists anyway.
        debug_assert_eq!(results.len(), node_ids.len());

        Ok(results)
    }

    /// Browses continuation points for more references.
    ///
    /// This uses continuation points returned from [`browse()`] and [`browse_many()`] whenever not
    /// all references were returned (due to client or server limits).
    ///
    /// The size and order of the result list matches the size and order of the given continuation
    /// point list.
    ///
    /// # Errors
    ///
    /// This fails when any of the given continuation points is invalid.
    ///
    /// [`browse()`]: Self::browse
    /// [`browse_many()`]: Self::browse_many
    pub async fn browse_next(
        &self,
        continuation_points: &[ua::ContinuationPoint],
    ) -> Result<Vec<Option<(Vec<ua::ReferenceDescription>, Option<ua::ContinuationPoint>)>>, Error>
    {
        let request = ua::BrowseNextRequest::init().with_continuation_points(continuation_points);

        let response = service_request(&self.client, request).await?;

        let Some(results) = response.results() else {
            return Err(Error::internal("browse should return results"));
        };

        let results: Vec<_> = results
            .iter()
            .map(|result| {
                result
                    .references()
                    .map(|references| (references.into_vec(), result.continuation_point()))
            })
            .collect();

        // The OPC UA specification state that the resulting list has the same number of elements as
        // the request list. If not, we would not be able to match elements in the two lists anyway.
        debug_assert_eq!(results.len(), continuation_points.len());

        Ok(results)
    }

    /// Creates new [subscription](AsyncSubscription).
    ///
    /// # Errors
    ///
    /// This fails when the client is not connected.
    pub async fn create_subscription(&self) -> Result<AsyncSubscription, Error> {
        AsyncSubscription::new(&self.client).await
    }
}

impl Drop for AsyncClient {
    fn drop(&mut self) {
        self.background_handle.abort();

        if let Ok(mut client) = self.client.lock() {
            let _unused = unsafe { UA_Client_disconnect(client.as_mut_ptr()) };
        }
    }
}

async fn background_task(client: Arc<Mutex<ua::Client>>, cycle_time: Duration) {
    log::debug!("Starting background task");

    let mut interval = time::interval(cycle_time);
    // TODO: Offer customized `MissedTickBehavior`? Only `Skip` and `Delay` are suitable here as we
    // don't want `Burst` to repeatedly and unnecessarily call `UA_Client_run_iterate()` many times
    // in a row.
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // `UA_Client_run_iterate()` must be run periodically and makes sure to maintain the connection
    // (e.g. renew session) and run callback handlers.
    loop {
        // This await point is where the background task could be aborted. (The first tick finishes
        // immediately, so there is no additional delay on the first iteration.)
        interval.tick().await;
        // Track time of cycle start to report missed cycles below.
        let start_of_cycle = Instant::now();

        let status_code = ua::StatusCode::new({
            let Ok(mut client) = client.lock() else {
                log::error!("Terminating background task: Client could not be locked");
                return;
            };

            // Timeout of 0 means we do not block here at all. We don't want to hold the mutex
            // longer than necessary (because that would block requests from being sent out).
            log::trace!("Running iterate");
            unsafe { UA_Client_run_iterate(client.as_mut_ptr(), 0) }
        });
        if let Err(error) = Error::verify_good(&status_code) {
            // Context-sensitive handling of bad status codes.
            match status_code.into_raw() {
                UA_STATUSCODE_BADDISCONNECT => {
                    // Not an error.
                    log::info!("Terminating background task after disconnect");
                }
                _ => {
                    // Unexpected error.
                    log::error!("Terminating background task: Run iterate failed with {error}");
                }
            }
            return;
        }

        let time_taken = start_of_cycle.elapsed();

        // Detect and log missed cycles.
        if !cycle_time.is_zero() && time_taken > cycle_time {
            let missed_cycles = time_taken.as_nanos() / cycle_time.as_nanos();
            log::warn!("Iterate run took {time_taken:?}, missed {missed_cycles} cycle(s)");
        } else {
            log::trace!("Iterate run took {time_taken:?}");
        }
    }
}

async fn service_request<R: ServiceRequest>(
    client: &Mutex<ua::Client>,
    request: R,
) -> Result<R::Response, Error> {
    type Cb<R> = CallbackOnce<Result<<R as ServiceRequest>::Response, ua::StatusCode>>;

    unsafe extern "C" fn callback_c<R: ServiceRequest>(
        _client: *mut UA_Client,
        userdata: *mut c_void,
        _request_id: UA_UInt32,
        response: *mut c_void,
    ) {
        log::debug!("Request completed");

        // SAFETY: Incoming pointer is valid for access.
        // PANIC: We expect pointer to be valid when good.
        let response = unsafe { response.cast::<<R::Response as DataType>::Inner>().as_ref() }
            .expect("response should be set");
        let response = R::Response::clone_raw(response);

        let status_code = response.service_result();
        let result = if status_code.is_good() {
            Ok(response)
        } else {
            Err(status_code)
        };

        // SAFETY: `userdata` is the result of `Cb::prepare()` and is used only once.
        unsafe {
            Cb::<R>::execute(userdata, result);
        }
    }

    let (tx, rx) = oneshot::channel::<Result<R::Response, Error>>();

    let callback = |result: Result<R::Response, _>| {
        // We always send a result back via `tx` (in fact, `rx.await` below expects this). We do not
        // care if that succeeds though: the receiver might already have gone out of scope (when its
        // future has been canceled) and we must not panic in FFI callbacks.
        let _unused = tx.send(result.map_err(Error::new));
    };

    let status_code = ua::StatusCode::new({
        let Ok(mut client) = client.lock() else {
            return Err(Error::internal("should be able to lock client"));
        };

        log::debug!("Calling request");

        unsafe {
            UA_Client_sendAsyncRequest(
                client.as_mut_ptr(),
                request.as_ptr().cast::<c_void>(),
                R::data_type(),
                Some(callback_c::<R>),
                R::Response::data_type(),
                Cb::<R>::prepare(callback),
                ptr::null_mut(),
            )
        }
    });
    Error::verify_good(&status_code)?;

    // PANIC: When `callback` is called (which owns `tx`), we always call `tx.send()`. So the sender
    // is only dropped after placing a value into the channel and `rx.await` always finds this value
    // there.
    rx.await
        .unwrap_or(Err(Error::internal("callback should send result")))
}
