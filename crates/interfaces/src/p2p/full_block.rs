use crate::p2p::{
    bodies::client::{BodiesClient, SingleBodyRequest},
    error::PeerRequestResult,
    headers::client::{HeadersClient, SingleHeaderRequest},
};
use reth_primitives::{BlockBody, Header, HeadersDirection, SealedBlock, SealedHeader, H256};
use std::{
    cmp::Reverse,
    fmt::Debug,
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};
use tracing::debug;

use super::headers::client::HeadersRequest;

/// A Client that can fetch full blocks from the network.
#[derive(Debug, Clone)]
pub struct FullBlockClient<Client> {
    client: Client,
}

impl<Client> FullBlockClient<Client> {
    /// Creates a new instance of `FullBlockClient`.
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl<Client> FullBlockClient<Client>
where
    Client: BodiesClient + HeadersClient + Clone,
{
    /// Returns a future that fetches the [SealedBlock] for the given hash.
    ///
    /// Note: this future is cancel safe
    ///
    /// Caution: This does no validation of body (transactions) response but guarantees that the
    /// [SealedHeader] matches the requested hash.
    pub fn get_full_block(&self, hash: H256) -> FetchFullBlockFuture<Client> {
        let client = self.client.clone();
        FetchFullBlockFuture {
            hash,
            request: FullBlockRequest {
                header: Some(client.get_header(hash.into())),
                body: Some(client.get_block_body(hash)),
            },
            client,
            header: None,
            body: None,
        }
    }

    /// Returns a future that fetches [SealedBlock]s for the given hash and count.
    ///
    /// Note: this future is cancel safe
    ///
    /// Caution: This does no validation of body (transactions) responses but guarantees that
    /// the starting [SealedHeader] matches the requested hash, and that the number of headers and
    /// bodies received matches the requested limit.
    pub fn get_full_block_range(
        &self,
        hash: H256,
        count: u64,
    ) -> FetchFullBlockRangeFuture<Client> {
        let client = self.client.clone();

        // Optimization: if we only want one block, we don't need to wait for the headers request
        // to complete, and can send the block bodies request right away.
        let bodies_request =
            if count == 1 { None } else { Some(client.get_block_bodies(vec![hash])) };

        FetchFullBlockRangeFuture {
            hash,
            count,
            request: FullBlockRangeRequest {
                headers: Some(client.get_headers(HeadersRequest {
                    start: hash.into(),
                    limit: count,
                    direction: HeadersDirection::Falling,
                })),
                bodies: bodies_request,
            },
            client,
            headers: None,
            bodies: None,
        }
    }
}

/// A future that downloads a full block from the network.
///
/// This will attempt to fetch both the header and body for the given block hash at the same time.
/// When both requests succeed, the future will yield the full block.
#[must_use = "futures do nothing unless polled"]
pub struct FetchFullBlockFuture<Client>
where
    Client: BodiesClient + HeadersClient,
{
    client: Client,
    hash: H256,
    request: FullBlockRequest<Client>,
    header: Option<SealedHeader>,
    body: Option<BlockBody>,
}

impl<Client> FetchFullBlockFuture<Client>
where
    Client: BodiesClient + HeadersClient,
{
    /// Returns the hash of the block being requested.
    pub fn hash(&self) -> &H256 {
        &self.hash
    }

    /// If the header request is already complete, this returns the block number
    pub fn block_number(&self) -> Option<u64> {
        self.header.as_ref().map(|h| h.number)
    }

    /// Returns the [SealedBlock] if the request is complete.
    fn take_block(&mut self) -> Option<SealedBlock> {
        if self.header.is_none() || self.body.is_none() {
            return None
        }
        let header = self.header.take().unwrap();
        let body = self.body.take().unwrap();

        Some(SealedBlock::new(header, body))
    }
}

impl<Client> Future for FetchFullBlockFuture<Client>
where
    Client: BodiesClient + HeadersClient + Unpin + 'static,
{
    type Output = SealedBlock;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match ready!(this.request.poll(cx)) {
                ResponseResult::Header(res) => {
                    match res {
                        Ok(maybe_header) => {
                            let (peer, maybe_header) =
                                maybe_header.map(|h| h.map(|h| h.seal_slow())).split();
                            if let Some(header) = maybe_header {
                                if header.hash() != this.hash {
                                    debug!(target: "downloaders", expected=?this.hash, received=?header.hash, "Received wrong header");
                                    // received bad header
                                    this.client.report_bad_message(peer)
                                } else {
                                    this.header = Some(header);
                                }
                            }
                        }
                        Err(err) => {
                            debug!(target: "downloaders", %err, ?this.hash, "Header download failed");
                        }
                    }

                    if this.header.is_none() {
                        // received bad response
                        this.request.header = Some(this.client.get_header(this.hash.into()));
                    }
                }
                ResponseResult::Body(res) => {
                    match res {
                        Ok(maybe_body) => {
                            this.body = maybe_body.into_data();
                        }
                        Err(err) => {
                            debug!(target: "downloaders", %err, ?this.hash, "Body download failed");
                        }
                    }
                    if this.body.is_none() {
                        // received bad response
                        this.request.body = Some(this.client.get_block_body(this.hash));
                    }
                }
            }

            if let Some(res) = this.take_block() {
                return Poll::Ready(res)
            }
        }
    }
}

impl<Client> Debug for FetchFullBlockFuture<Client>
where
    Client: BodiesClient + HeadersClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchFullBlockFuture")
            .field("hash", &self.hash)
            .field("header", &self.header)
            .field("body", &self.body)
            .finish()
    }
}

struct FullBlockRequest<Client>
where
    Client: BodiesClient + HeadersClient,
{
    header: Option<SingleHeaderRequest<<Client as HeadersClient>::Output>>,
    body: Option<SingleBodyRequest<<Client as BodiesClient>::Output>>,
}

impl<Client> FullBlockRequest<Client>
where
    Client: BodiesClient + HeadersClient,
{
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ResponseResult> {
        if let Some(fut) = Pin::new(&mut self.header).as_pin_mut() {
            if let Poll::Ready(res) = fut.poll(cx) {
                self.header = None;
                return Poll::Ready(ResponseResult::Header(res))
            }
        }

        if let Some(fut) = Pin::new(&mut self.body).as_pin_mut() {
            if let Poll::Ready(res) = fut.poll(cx) {
                self.body = None;
                return Poll::Ready(ResponseResult::Body(res))
            }
        }

        Poll::Pending
    }
}

/// The result of a request for a single header or body. This is yielded by the `FullBlockRequest`
/// future.
enum ResponseResult {
    Header(PeerRequestResult<Option<Header>>),
    Body(PeerRequestResult<Option<BlockBody>>),
}

/// A future that downloads a range of full blocks from the network.
///
/// This first fetches the headers for the given range using the inner `Client`. Once the request
/// is complete, it will fetch the bodies for the headers it received.
///
/// Once the bodies request completes, the [SealedBlock]s will be assembled and the future will
/// yield the full block range.
#[must_use = "futures do nothing unless polled"]
pub struct FetchFullBlockRangeFuture<Client>
where
    Client: BodiesClient + HeadersClient,
{
    client: Client,
    hash: H256,
    count: u64,
    request: FullBlockRangeRequest<Client>,
    headers: Option<Vec<SealedHeader>>,
    bodies: Option<Vec<BlockBody>>,
}

impl<Client> FetchFullBlockRangeFuture<Client>
where
    Client: BodiesClient + HeadersClient,
{
    /// Returns the block hashes for the given range, if they are available.
    pub fn range_block_hashes(&self) -> Option<Vec<H256>> {
        self.headers.as_ref().map(|h| h.iter().map(|h| h.hash()).collect::<Vec<_>>())
    }

    /// Returns the [SealedBlock]s if the request is complete.
    fn take_blocks(&mut self) -> Option<Vec<SealedBlock>> {
        if self.headers.is_none() || self.bodies.is_none() {
            return None
        }

        let headers = self.headers.take().unwrap();
        let bodies = self.bodies.take().unwrap();
        Some(
            headers
                .iter()
                .zip(bodies.iter())
                .map(|(h, b)| SealedBlock::new(h.clone(), b.clone()))
                .collect::<Vec<_>>(),
        )
    }

    /// Returns whether or not a bodies request has been started, by making sure there is no
    /// pending request, and that there is no buffered response.
    fn has_bodies_request_started(&self) -> bool {
        self.request.bodies.is_none() && self.bodies.is_none()
    }
}

impl<Client> Future for FetchFullBlockRangeFuture<Client>
where
    Client: BodiesClient + HeadersClient + Unpin + 'static,
{
    type Output = Vec<SealedBlock>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match ready!(this.request.poll(cx)) {
                // This branch handles headers responses from peers - it first ensures that the
                // starting hash and number of headers matches what we requested.
                //
                // If these don't match, we penalize the peer and retry the request.
                // If they do match, we sort the headers by block number and start the request for
                // the corresponding block bodies.
                //
                // The next result that should be yielded by `poll` is the bodies response.
                RangeResponseResult::Header(res) => {
                    match res {
                        Ok(headers) => {
                            let (peer, mut headers) = headers
                                .map(|h| {
                                    h.iter().map(|h| h.clone().seal_slow()).collect::<Vec<_>>()
                                })
                                .split();

                            // ensure the response is what we requested
                            if headers.is_empty() || (headers.len() as u64) != this.count {
                                // received bad response
                                this.client.report_bad_message(peer);
                            } else {
                                // sort headers from highest to lowest block number
                                headers.sort_unstable_by_key(|h| Reverse(h.number));

                                // check the starting hash
                                if headers[0].hash() != this.hash {
                                    // received bad response
                                    this.client.report_bad_message(peer);
                                } else {
                                    // get the bodies request so it can be polled later
                                    let hashes =
                                        headers.iter().map(|h| h.hash()).collect::<Vec<_>>();

                                    // set the actual request if it hasn't been started yet
                                    if !this.has_bodies_request_started() {
                                        this.request.bodies =
                                            Some(this.client.get_block_bodies(hashes));
                                    }

                                    // set the headers response
                                    this.headers = Some(headers);
                                }
                            }
                        }
                        Err(err) => {
                            debug!(target: "downloaders", %err, ?this.hash, "Header range download failed");
                        }
                    }

                    if this.headers.is_none() {
                        // received bad response, retry
                        this.request.headers = Some(this.client.get_headers(HeadersRequest {
                            start: this.hash.into(),
                            limit: this.count,
                            direction: HeadersDirection::Falling,
                        }));
                    }
                }
                // This branch handles block body responses from peers - it first checks that the
                // number of bodies matches what we requested.
                //
                // If the number of bodies doesn't match, we penalize the peer and retry the
                // request.
                // If the number of bodies does match, we assemble the bodies with the headers
                // received by a previous response, and return the result.
                RangeResponseResult::Body(res) => {
                    match res {
                        Ok(bodies_resp) => {
                            let (peer, bodies) = bodies_resp.split();
                            if bodies.len() != this.count as usize {
                                // received bad response
                                this.client.report_bad_message(peer);
                            } else {
                                this.bodies = Some(bodies);
                            }
                        }
                        Err(err) => {
                            debug!(target: "downloaders", %err, ?this.hash, "Body range download failed");
                        }
                    }
                    if this.bodies.is_none() {
                        // received bad response, re-request headers
                        // TODO: convert this into two futures, one which is a headers range
                        // future, and one which is a bodies range future.
                        //
                        // The headers range future should yield the bodies range future.
                        // The bodies range future should not have an Option<Vec<H256>>, it should
                        // have a populated Vec<H256> from the successful headers range future.
                        //
                        // This is optimal because we can not send a bodies request without
                        // first completing the headers request. This way we can get rid of the
                        // following `if let Some`. A bodies request should never be sent before
                        // the headers request completes, so this should always be `Some` anyways.
                        if let Some(hashes) = this.range_block_hashes() {
                            this.request.bodies = Some(this.client.get_block_bodies(hashes));
                        }
                    }
                }
            }

            if let Some(res) = this.take_blocks() {
                return Poll::Ready(res)
            }
        }
    }
}

/// A request for a range of full blocks. Polling this will poll the inner headers and bodies
/// futures until they return responses. It will return either the header or body result, depending
/// on which future successfully returned.
struct FullBlockRangeRequest<Client>
where
    Client: BodiesClient + HeadersClient,
{
    headers: Option<<Client as HeadersClient>::Output>,
    bodies: Option<<Client as BodiesClient>::Output>,
}

impl<Client> FullBlockRangeRequest<Client>
where
    Client: BodiesClient + HeadersClient,
{
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<RangeResponseResult> {
        if let Some(fut) = Pin::new(&mut self.headers).as_pin_mut() {
            if let Poll::Ready(res) = fut.poll(cx) {
                self.headers = None;
                return Poll::Ready(RangeResponseResult::Header(res))
            }
        }

        if let Some(fut) = Pin::new(&mut self.bodies).as_pin_mut() {
            if let Poll::Ready(res) = fut.poll(cx) {
                self.bodies = None;
                return Poll::Ready(RangeResponseResult::Body(res))
            }
        }

        Poll::Pending
    }
}

// The result of a request for headers or block bodies. This is yielded by the
// `FullBlockRangeRequest` future.
enum RangeResponseResult {
    Header(PeerRequestResult<Vec<Header>>),
    Body(PeerRequestResult<Vec<BlockBody>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::{
        download::DownloadClient, headers::client::HeadersRequest, priority::Priority,
    };
    use parking_lot::Mutex;
    use reth_primitives::{BlockHashOrNumber, BlockNumHash, PeerId, WithPeerId};
    use std::{collections::HashMap, sync::Arc};

    #[derive(Clone, Default, Debug)]
    struct TestFullBlockClient {
        headers: Arc<Mutex<HashMap<H256, Header>>>,
        bodies: Arc<Mutex<HashMap<H256, BlockBody>>>,
    }

    impl TestFullBlockClient {
        fn insert(&self, header: SealedHeader, body: BlockBody) {
            let hash = header.hash();
            let header = header.unseal();
            self.headers.lock().insert(hash, header);
            self.bodies.lock().insert(hash, body);
        }
    }

    impl DownloadClient for TestFullBlockClient {
        fn report_bad_message(&self, _peer_id: PeerId) {}

        fn num_connected_peers(&self) -> usize {
            1
        }
    }

    impl HeadersClient for TestFullBlockClient {
        type Output = futures::future::Ready<PeerRequestResult<Vec<Header>>>;

        fn get_headers_with_priority(
            &self,
            request: HeadersRequest,
            _priority: Priority,
        ) -> Self::Output {
            let headers = self.headers.lock();
            let mut block: BlockHashOrNumber = match request.start {
                BlockHashOrNumber::Hash(hash) => headers.get(&hash).cloned(),
                BlockHashOrNumber::Number(num) => {
                    headers.values().find(|h| h.number == num).cloned()
                }
            }
            .map(|h| h.number.into())
            .unwrap();

            let mut resp = Vec::new();

            for _ in 0..request.limit {
                // fetch from storage
                if let Some((_, header)) = headers.iter().find(|(hash, header)| {
                    BlockNumHash::new(header.number, **hash).matches_block_or_num(&block)
                }) {
                    match request.direction {
                        HeadersDirection::Falling => block = header.parent_hash.into(),
                        HeadersDirection::Rising => {
                            let next = header.number + 1;
                            block = next.into()
                        }
                    }
                    resp.push(header.clone());
                } else {
                    break
                }
            }
            futures::future::ready(Ok(WithPeerId::new(PeerId::random(), resp)))
        }
    }

    impl BodiesClient for TestFullBlockClient {
        type Output = futures::future::Ready<PeerRequestResult<Vec<BlockBody>>>;

        fn get_block_bodies_with_priority(
            &self,
            hashes: Vec<H256>,
            _priority: Priority,
        ) -> Self::Output {
            let bodies = self.bodies.lock();
            let mut all_bodies = Vec::new();
            for hash in hashes {
                if let Some(body) = bodies.get(&hash) {
                    all_bodies.push(body.clone());
                }
            }
            futures::future::ready(Ok(WithPeerId::new(PeerId::random(), all_bodies)))
        }
    }

    #[tokio::test]
    async fn download_single_full_block() {
        let client = TestFullBlockClient::default();
        let header = SealedHeader::default();
        let body = BlockBody::default();
        client.insert(header.clone(), body.clone());
        let client = FullBlockClient::new(client);

        let received = client.get_full_block(header.hash()).await;
        assert_eq!(received, SealedBlock::new(header, body));
    }

    #[tokio::test]
    async fn download_single_full_block_range() {
        let client = TestFullBlockClient::default();
        let header = SealedHeader::default();
        let body = BlockBody::default();
        client.insert(header.clone(), body.clone());
        let client = FullBlockClient::new(client);

        let received = client.get_full_block_range(header.hash(), 1).await;
        let received = received.first().expect("response should include a block");
        assert_eq!(*received, SealedBlock::new(header, body));
    }

    #[tokio::test]
    async fn download_full_block_range() {
        let client = TestFullBlockClient::default();
        let mut header = SealedHeader::default();
        let body = BlockBody::default();
        client.insert(header.clone(), body.clone());
        for _ in 0..10 {
            header.parent_hash = header.hash_slow();
            header.number += 1;
            header = header.header.seal_slow();
            client.insert(header.clone(), body.clone());
        }
        let client = FullBlockClient::new(client);

        let received = client.get_full_block_range(header.hash(), 1).await;
        let received = received.first().expect("response should include a block");
        assert_eq!(*received, SealedBlock::new(header.clone(), body));

        let received = client.get_full_block_range(header.hash(), 10).await;
        assert_eq!(received.len(), 10);
        for (i, block) in received.iter().enumerate() {
            let expected_number = header.number - i as u64;
            assert_eq!(block.header.number, expected_number);
        }
    }
}
