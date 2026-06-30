#[cfg(target_arch = "wasm32")]
mod gloo_client;
#[cfg(target_arch = "wasm32")]
mod util;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use crate::gloo_client::{WasmBufferedHttpRangeClient64, WasmHttpClient};

    use fcb_core::{size_prefixed_root_as_header, Header, Operator};
    use js_sys::Array;

    use log::Level;
    use serde::Serialize;
    use wasm_bindgen::prelude::*;

    use fcb_core::static_btree::{FixedStringKey, Float, HttpIndex, HttpMultiIndex, KeyType};

    use byteorder::{ByteOrder, LittleEndian};
    use bytes::{BufMut, Bytes, BytesMut};
    use chrono::{DateTime, Utc};
    use fcb_core::city_buffer::FcbBuffer;
    use fcb_core::{
        build_query, check_magic_bytes,
        deserializer::{to_cj_feature, to_cj_metadata},
        fb::*,
        size_prefixed_root_as_city_feature, AttrQuery, HEADER_MAX_BUFFER_SIZE, HEADER_SIZE_SIZE,
        MAGIC_BYTES_SIZE,
    };

    use std::result::Result;
    use std::sync::atomic::{AtomicBool, Ordering};

    use http_range_client::{AsyncBufferedHttpRangeClient, AsyncHttpRangeClient};

    use fcb_core::packed_rtree::{http::HttpRange, NodeItem, PackedRTree, Query as SpatialQuery};
    use std::collections::VecDeque;
    use std::ops::{Range, RangeFrom};

    // The largest request we'll speculatively make.
    // If a single huge feature requires, we'll necessarily exceed this limit.
    const DEFAULT_HTTP_FETCH_SIZE: usize = 1_048_576; // 1MB

    // Static variable to track if logger has been initialized
    static LOGGER_INITIALIZED: AtomicBool = AtomicBool::new(false);

    /// FlatCityBuf dataset HTTP reader
    #[wasm_bindgen]
    pub struct HttpFcbReader {
        client: AsyncBufferedHttpRangeClient<WasmHttpClient>,
        client64: WasmBufferedHttpRangeClient64,
        // feature reading requires header access, therefore
        // header_buf is included in the FcbBuffer struct.
        fbs: FcbBuffer,
    }

    #[wasm_bindgen]
    pub struct AsyncFeatureIter {
        client: AsyncBufferedHttpRangeClient<WasmHttpClient>,
        client64: WasmBufferedHttpRangeClient64,
        // feature reading requires header access, therefore
        // header_buf is included in the FcbBuffer struct.
        fbs: FcbBuffer,
        /// Which features to iterate
        selection: FeatureSelection,
        /// Number of selected features
        count: usize,
    }

    #[wasm_bindgen(start)]
    impl HttpFcbReader {
        #[wasm_bindgen(constructor, start)]
        pub async fn new(url: String) -> Result<HttpFcbReader, JsValue> {
            console_error_panic_hook::set_once();

            // Only initialize the logger once
            if !LOGGER_INITIALIZED.load(Ordering::SeqCst)
                && console_log::init_with_level(Level::Info).is_ok()
            {
                LOGGER_INITIALIZED.store(true, Ordering::SeqCst);
                log::info!("Logger initialized successfully.");
            }

            let client = WasmHttpClient::new(&url);
            let client64 = WasmBufferedHttpRangeClient64::new(&url);

            Self::_open(client, client64).await
        }

        async fn _open(
            mut client: AsyncBufferedHttpRangeClient<WasmHttpClient>,
            client64: WasmBufferedHttpRangeClient64,
        ) -> Result<HttpFcbReader, JsValue> {
            // Because we use a buffered HTTP reader, anything extra we fetch here can
            // be utilized to skip subsequent fetches.
            // Immediately following the header is the optional spatial index, we deliberately fetch
            // a small part of that to skip subsequent requests
            let prefetch_index_bytes: usize = {
                // The actual branching factor will be in the header, but since we don't have the header
                // yet we guess. The consequence of getting this wrong isn't catastrophic, it just means
                // we may be fetching slightly more than we need or that we make an extra request later.
                let assumed_branching_factor = PackedRTree::DEFAULT_NODE_SIZE as usize;

                // NOTE: each layer is exponentially larger
                let prefetched_layers: u32 = 3;

                (0..prefetched_layers)
                    .map(|i| assumed_branching_factor.pow(i) * std::mem::size_of::<NodeItem>())
                    .sum()
            };

            // In reality, the header is probably less than half this size, but better to overshoot and
            // fetch an extra kb rather than have to issue a second request.
            let assumed_header_size = 4096;
            let min_req_size = assumed_header_size + prefetch_index_bytes;
            client.set_min_req_size(min_req_size);
            let mut read_bytes = 0;
            let bytes = client
                .get_range(read_bytes, MAGIC_BYTES_SIZE)
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?; // to get magic bytes
            if !check_magic_bytes(bytes) {
                return Err(JsValue::from_str("MissingMagicBytes"));
            }

            read_bytes += MAGIC_BYTES_SIZE;
            let mut bytes = BytesMut::from(
                client
                    .get_range(read_bytes, HEADER_SIZE_SIZE)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            read_bytes += HEADER_SIZE_SIZE;

            let header_size = LittleEndian::read_u32(&bytes) as usize;
            if !(8..=HEADER_MAX_BUFFER_SIZE).contains(&header_size) {
                // minimum size check avoids panic in FlatBuffers header decoding
                return Err(JsValue::from_str(&format!(
                    "IllegalHeaderSize: {header_size}"
                )));
            }

            bytes.put(
                client
                    .get_range(read_bytes, header_size)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            read_bytes += header_size;

            let header_buf = bytes.to_vec();
            // verify flatbuffer
            let header = size_prefixed_root_as_header(&header_buf)
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            Ok(HttpFcbReader {
                client,
                client64,
                fbs: FcbBuffer {
                    header_buf,
                    features_buf: Vec::new(),
                },
            })
        }

        #[wasm_bindgen]
        pub fn cityjson(&self) -> Result<JsValue, JsValue> {
            let header = self.fbs.header();
            let cj = to_cj_metadata(&header).map_err(|e| JsValue::from_str(&e.to_string()))?;
            let jsval = to_js_value(&cj)?;
            Ok(jsval)
        }

        #[wasm_bindgen]
        pub fn meta(&self) -> Result<JsValue, JsValue> {
            let meta = self
                .fbs
                .meta()
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            let js = to_js_value(&meta)?;
            Ok(js)
        }

        /// Return the spatial extent stored in the root R-tree node.
        ///
        /// This is available for indexed files even when the CityJSON metadata has
        /// no geographical extent.
        #[wasm_bindgen]
        pub async fn spatial_extent(&mut self) -> Result<JsValue, JsValue> {
            let index_node_size = self.fbs.header().index_node_size();
            let features_count = self.fbs.header().features_count();
            if index_node_size == 0 || features_count == 0 {
                return Err(JsValue::from_str("NoIndex"));
            }

            let root = self.root_rtree_node().await?;

            let result = js_sys::Object::new();
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str("minX"),
                &JsValue::from_f64(root.min_x),
            )?;
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str("minY"),
                &JsValue::from_f64(root.min_y),
            )?;
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str("maxX"),
                &JsValue::from_f64(root.max_x),
            )?;
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str("maxY"),
                &JsValue::from_f64(root.max_y),
            )?;

            Ok(result.into())
        }

        async fn root_rtree_node(&mut self) -> Result<NodeItem, JsValue> {
            let index_begin = self.header_len();
            let bytes = self
                .client64
                .min_req_size(0)
                .get_range(index_begin as u64, std::mem::size_of::<NodeItem>())
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            let mut cursor = std::io::Cursor::new(bytes.as_ref());
            NodeItem::from_reader(&mut cursor).map_err(|e| JsValue::from_str(&e.to_string()))
        }

        fn header_len(&self) -> usize {
            MAGIC_BYTES_SIZE + self.fbs.header_buf.len()
        }

        fn rtree_index_size(&self) -> Result<usize, JsValue> {
            u64_to_usize_file_offset(self.rtree_index_size_u64(), "R-tree index")
        }

        fn rtree_index_size_u64(&self) -> u64 {
            let header = self.fbs.header();
            let feat_count = header.features_count() as usize;
            if header.index_node_size() > 0 && feat_count > 0 {
                PackedRTree::index_size(feat_count, header.index_node_size()) as u64
            } else {
                0
            }
        }

        fn attr_index_size(&self) -> Result<usize, JsValue> {
            u64_to_usize_file_offset(self.attr_index_size_u64(), "attribute index")
        }

        fn attr_index_size_u64(&self) -> u64 {
            self.fbs
                .header()
                .attribute_index()
                .map(|attr_index| attr_index.iter().map(|ai| ai.length() as u64).sum())
                .unwrap_or(0)
        }

        fn feature_begin_u64(&self) -> u64 {
            self.header_len() as u64 + self.rtree_index_size_u64() + self.attr_index_size_u64()
        }

        /// Select all features.
        #[wasm_bindgen]
        pub async fn select_all(self) -> Result<AsyncFeatureIter, JsValue> {
            let header = self.fbs.header();
            let count = header.features_count();
            // TODO: support reading with unknown feature count
            let feature_base = self.feature_begin_u64();
            Ok(AsyncFeatureIter {
                client: self.client,
                client64: self.client64,
                fbs: self.fbs,
                selection: FeatureSelection::SelectAll(SelectAll {
                    features_left: count,
                    pos: feature_base,
                }),
                count: count as usize,
            })
        }
        /// Select features within a bounding box.
        #[wasm_bindgen]
        pub async fn select_spatial(
            self,
            query: &WasmSpatialQuery,
        ) -> Result<AsyncFeatureIter, JsValue> {
            self.select_spatial_paged(query, None, None).await
        }

        /// Select features within a bounding box with optional pagination.
        /// If `limit`/`offset` are provided, only a page of features is returned while
        /// `features_count()` on the returned iterator still reflects the total number of matches.
        #[wasm_bindgen]
        pub async fn select_spatial_paged(
            mut self,
            query: &WasmSpatialQuery,
            limit: Option<u32>,
            offset: Option<u32>,
        ) -> Result<AsyncFeatureIter, JsValue> {
            let (count, selection) = self.spatial_selection(query, limit, offset).await?;

            Ok(AsyncFeatureIter {
                client: self.client,
                client64: self.client64,
                fbs: self.fbs,
                selection,
                count,
            })
        }

        /// Query spatially selected features without consuming the reader.
        ///
        /// Returns `{ totalCount, features }`, where `totalCount` is the number of
        /// features matching the spatial query before pagination and `features`
        /// contains the decoded CityJSONFeature objects for the requested page.
        #[wasm_bindgen]
        pub async fn query_spatial_features(
            &mut self,
            query: &WasmSpatialQuery,
            limit: Option<u32>,
            offset: Option<u32>,
        ) -> Result<JsValue, JsValue> {
            let (total_count, mut selection) = self.spatial_selection(query, limit, offset).await?;
            let features = Array::new();

            while let Some(buffer) = selection
                .next_feature_buffer(&mut self.client, &mut self.client64)
                .await?
            {
                self.fbs.features_buf = buffer.to_vec();
                let feature = size_prefixed_root_as_city_feature(&self.fbs.features_buf)
                    .map_err(|e| JsValue::from_str(&e.to_string()))?;
                let header = self.fbs.header();
                let cj_feature =
                    to_cj_feature(feature, header.columns(), header.semantic_columns())
                        .map_err(|e| JsValue::from_str(&e.to_string()))?;

                features.push(&to_js_value(&cj_feature)?);
            }

            let result = js_sys::Object::new();
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str("totalCount"),
                &JsValue::from_f64(total_count as f64),
            )?;
            js_sys::Reflect::set(&result, &JsValue::from_str("features"), &features)?;
            Ok(result.into())
        }

        async fn spatial_selection(
            &mut self,
            query: &WasmSpatialQuery,
            limit: Option<u32>,
            offset: Option<u32>,
        ) -> Result<(usize, FeatureSelection), JsValue> {
            // Read R-Tree index and build filter for features within bbox
            let header = self.fbs.header();
            let index_node_size = header.index_node_size();
            let features_count = header.features_count();
            if index_node_size == 0 || features_count == 0 {
                return Err(JsValue::from_str("NoIndex"));
            }
            let count = features_count as usize;
            let header_len = self.header_len();

            // request up to this many extra bytes if it means we can eliminate an extra request
            let combine_request_threshold = 256 * 1024;
            let attr_index_size = self.attr_index_size_u64();

            // Clone the inner query value
            let inner_query = query.get_inner();

            if let (Some(limit), 0, SpatialQuery::BBox(min_x, min_y, max_x, max_y)) =
                (limit, offset.unwrap_or(0), inner_query)
            {
                let root = self.root_rtree_node().await?;
                if bbox_covers_node(min_x, min_y, max_x, max_y, &root) {
                    let feature_base = self.feature_begin_u64();
                    return Ok((
                        count,
                        FeatureSelection::SelectAll(SelectAll {
                            features_left: (limit as u64).min(features_count),
                            pos: feature_base,
                        }),
                    ));
                }
            }

            let page = http_stream_search64(
                &mut self.client64,
                header_len as u64,
                attr_index_size,
                count,
                PackedRTree::DEFAULT_NODE_SIZE,
                query.get_inner(),
                combine_request_threshold,
                limit.map(|value| value as usize),
                offset.unwrap_or(0) as usize,
            )
            .await?;
            debug_assert!(
                page.items
                    .windows(2)
                    .all(|w| w[0].range.start() < w[1].range.start()),
                "Since the tree is traversed breadth first, list should be sorted by construction."
            );

            let total_count = page.total_count;

            let feature_batches =
                FeatureBatch::make_batches(page.items, combine_request_threshold).await?;
            let selection = FeatureSelection::SelectSpatial(SelectSpatial { feature_batches });
            Ok((total_count, selection))
        }

        #[wasm_bindgen]
        pub async fn select_attr_query(
            self,
            query: &WasmAttrQuery,
        ) -> Result<AsyncFeatureIter, JsValue> {
            self.select_attr_query_paged(query, None, None).await
        }

        /// Attribute query with optional pagination.
        #[wasm_bindgen]
        pub async fn select_attr_query_paged(
            mut self,
            query: &WasmAttrQuery,
            limit: Option<u32>,
            offset: Option<u32>,
        ) -> Result<AsyncFeatureIter, JsValue> {
            let header = self.fbs.header();
            let header_len = self.header_len();
            // Assume the header provides rtree and attribute index sizes.

            // file structure:
            // magic_bytes + header + rtree_index + attr_index1 + attr_index2 + ... + features
            let rtree_index_size = self.rtree_index_size()?;
            let attr_index_size = self.attr_index_size()?;
            let attr_index_begin =
                checked_file_offset_add(header_len, rtree_index_size, "attribute index")?;
            let feature_begin =
                checked_file_offset_add(attr_index_begin, attr_index_size, "feature data")?;

            let combine_request_threshold = 1024 * 1024; // TODO: make this configurable
            let attr_index_entries = header
                .attribute_index()
                .ok_or_else(|| JsValue::from_str("attribute index not found"))?;
            let mut attr_index_entries = attr_index_entries.iter().collect::<Vec<_>>();
            let columns: Vec<Column> = header
                .columns()
                .ok_or_else(|| JsValue::from_str("no columns in header"))?
                .iter()
                .collect();
            attr_index_entries.sort_by_key(|attr_info| attr_info.index());

            // Build the query
            let query = build_query(&query.inner);

            // Create a StreamableMultiIndex from HTTP range requests
            let mut http_multi_index = HttpMultiIndex::new();
            let mut current_index_begin = attr_index_begin;
            for attr_info in attr_index_entries.iter() {
                Self::add_indices_to_multi_http_index(
                    &mut http_multi_index,
                    &columns,
                    attr_info,
                    current_index_begin,
                    feature_begin,
                    combine_request_threshold,
                )
                .map_err(|e| JsValue::from_str(&format!("failed to add index: {e:?}")))?;
                current_index_begin = checked_file_offset_add(
                    current_index_begin,
                    u64_to_usize_file_offset(attr_info.length().into(), "attribute index")?,
                    "attribute index",
                )?;
            }
            // self.client.set_min_req_size(combine_request_threshold);
            let result = http_multi_index
                .query(&mut self.client, &query.conditions)
                .await
                .map_err(|e| JsValue::from_str(&format!("failed to query index: {e:?}")))?;

            let total_count = result.len();

            // Apply pagination
            let start = offset.unwrap_or(0) as usize;
            let start = start.min(total_count);
            let end = match limit {
                Some(l) => start.saturating_add(l as usize).min(total_count),
                None => total_count,
            };
            let paged_iter: Vec<_> = if start < end {
                result.into_iter().skip(start).take(end - start).collect()
            } else {
                Vec::new()
            };

            let http_ranges: Vec<HttpRange> = paged_iter
                .into_iter()
                .map(|item| match item.range {
                    fcb_core::static_btree::http::HttpRange::Range(range) => {
                        HttpRange::Range(range.start..range.end)
                    }
                    fcb_core::static_btree::http::HttpRange::RangeFrom(range) => {
                        HttpRange::RangeFrom(range.start..)
                    }
                })
                .collect();

            Ok(AsyncFeatureIter {
                client: self.client,
                client64: self.client64,
                fbs: self.fbs,
                selection: FeatureSelection::SelectAttr(SelectAttr {
                    ranges: http_ranges,
                    range_pos: 0,
                }),
                count: total_count,
            })
        }

        fn add_indices_to_multi_http_index(
            multi_index: &mut HttpMultiIndex<WasmHttpClient>,
            columns: &[Column],
            attr_info: &AttributeIndex,
            index_begin: usize,
            feature_begin: usize,
            combine_request_threshold: usize,
        ) -> Result<(), JsValue> {
            if let Some(col) = columns.iter().find(|col| col.index() == attr_info.index()) {
                // TODO: now it assuming to add all indices to the multi_index. However, we should only add the indices that are used in the query. To do that, we need to change the implementation of StreamMultiIndex. Current StreamMultiIndex's `add_index` method assumes that all indices are added to the multi_index. We'll change it to take Range<usize> as an argument.
                let index_begin = index_begin;

                match col.type_() {
                    ColumnType::Int => {
                        let index = HttpIndex::<i32>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::Float => {
                        let index = HttpIndex::<Float<f32>>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::Double => {
                        let index = HttpIndex::<Float<f64>>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::String => {
                        let index = HttpIndex::<FixedStringKey<50>>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }

                    ColumnType::Bool => {
                        let index = HttpIndex::<bool>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::DateTime => {
                        let index = HttpIndex::<chrono::DateTime<chrono::Utc>>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::Short => {
                        let index = HttpIndex::<i16>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }

                    ColumnType::UShort => {
                        let index = HttpIndex::<u16>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::UInt => {
                        let index = HttpIndex::<u32>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::ULong => {
                        let index = HttpIndex::<u64>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::Byte => {
                        let index = HttpIndex::<i8>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    ColumnType::UByte => {
                        let index = HttpIndex::<u8>::new(
                            attr_info.num_unique_items() as usize,
                            attr_info.branching_factor(),
                            index_begin,
                            feature_begin,
                            combine_request_threshold,
                        );
                        multi_index.add_index(col.name().to_string(), index);
                    }
                    _ => {
                        return Err(JsValue::from_str(&format!(
                            "Unsupported column type: {:?}",
                            col.type_()
                        )))
                    }
                }
            }
            Ok(())
        }
    }

    #[wasm_bindgen]
    impl AsyncFeatureIter {
        fn _header(&self) -> Header {
            self.fbs.header()
        }
        #[wasm_bindgen]
        pub fn header(&self) -> Result<JsValue, JsValue> {
            let header = self.fbs.header();
            let cj = to_cj_metadata(&header).map_err(|e| JsValue::from_str(&e.to_string()))?;
            to_js_value(&cj)
        }
        /// Number of selected features (might be unknown)
        #[wasm_bindgen]
        pub fn features_count(&self) -> Option<usize> {
            if self.count > 0 {
                Some(self.count)
            } else {
                None
            }
        }
        /// Read next feature
        #[wasm_bindgen]
        pub async fn next(&mut self) -> Result<Option<JsValue>, JsValue> {
            let Some(buffer) = self
                .selection
                .next_feature_buffer(&mut self.client, &mut self.client64)
                .await?
            else {
                return Ok(None);
            };

            // Not zero-copy
            self.fbs.features_buf = buffer.to_vec();
            // verify flatbuffer
            let feature = size_prefixed_root_as_city_feature(&self.fbs.features_buf)
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            let cj_feature = to_cj_feature(
                feature,
                self._header().columns(),
                self._header().semantic_columns(),
            )
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

            Ok(Some(to_js_value(&cj_feature)?))
        }

        #[wasm_bindgen]
        pub fn cur_cj_feature(&self) -> Result<JsValue, JsValue> {
            let cj_feature = to_cj_feature(
                self.fbs.feature(),
                self._header().columns(),
                self._header().semantic_columns(),
            )
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
            Ok(to_js_value(&cj_feature)?)
        }
    }

    fn to_js_value<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
        let json = serde_json::to_string(value).map_err(|e| JsValue::from_str(&e.to_string()))?;
        js_sys::JSON::parse(&json)
    }

    fn bbox_covers_node(min_x: f64, min_y: f64, max_x: f64, max_y: f64, node: &NodeItem) -> bool {
        const EPSILON: f64 = 1.0e-9;
        min_x <= node.min_x + EPSILON
            && min_y <= node.min_y + EPSILON
            && max_x >= node.max_x - EPSILON
            && max_y >= node.max_y - EPSILON
    }

    fn u64_to_usize_file_offset(value: u64, context: &str) -> Result<usize, JsValue> {
        usize::try_from(value).map_err(|_| large_file_offset_error(context))
    }

    fn checked_file_offset_add(left: usize, right: usize, context: &str) -> Result<usize, JsValue> {
        left.checked_add(right)
            .ok_or_else(|| large_file_offset_error(context))
    }

    fn large_file_offset_error(context: &str) -> JsValue {
        JsValue::from_str(&format!(
            "{context} offset exceeds the current WASM reader limit. The existing FlatCityBuf WASM bindings use 32-bit usize file offsets, so indexed files whose feature data starts past 4GB need a u64-based HTTP/range reader before they can be queried in the browser."
        ))
    }

    #[derive(Debug, Clone)]
    enum HttpRange64 {
        Range(Range<u64>),
        RangeFrom(RangeFrom<u64>),
    }

    impl HttpRange64 {
        fn start(&self) -> u64 {
            match self {
                Self::Range(range) => range.start,
                Self::RangeFrom(range) => range.start,
            }
        }

        fn length(&self) -> Option<u64> {
            match self {
                Self::Range(range) => Some(range.end - range.start),
                Self::RangeFrom(_) => None,
            }
        }
    }

    #[derive(Debug)]
    struct HttpSearchResultItem64 {
        range: HttpRange64,
    }

    #[derive(Debug)]
    struct SpatialSearchPage64 {
        total_count: usize,
        items: Vec<HttpSearchResultItem64>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct NodeRange {
        level: usize,
        nodes: Range<usize>,
        search_end: usize,
    }

    async fn http_stream_search64(
        client: &mut WasmBufferedHttpRangeClient64,
        index_begin: u64,
        attr_index_size: u64,
        num_items: usize,
        branching_factor: u16,
        query: SpatialQuery,
        combine_request_threshold: usize,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<SpatialSearchPage64, JsValue> {
        if num_items == 0 {
            return Ok(SpatialSearchPage64 {
                total_count: 0,
                items: Vec::new(),
            });
        }

        let level_bounds = generate_level_bounds(num_items, branching_factor);
        let feature_begin = index_begin
            .checked_add(attr_index_size)
            .and_then(|offset| {
                offset.checked_add(PackedRTree::index_size(num_items, branching_factor) as u64)
            })
            .ok_or_else(|| JsValue::from_str("feature data offset overflow"))?;

        match query {
            SpatialQuery::BBox(min_x, min_y, max_x, max_y) => {
                let bounds = NodeItem::bounds(min_x, min_y, max_x, max_y);
                let mut queue = VecDeque::new();
                queue.push_back(NodeRange {
                    nodes: 0..1,
                    search_end: 1,
                    level: level_bounds.len() - 1,
                });
                let mut total_count = 0usize;
                let mut items = Vec::new();
                let page_limit = limit.unwrap_or(usize::MAX);

                while let Some(node_range) = queue.pop_front() {
                    let node_items =
                        read_http_node_items64(client, index_begin, &node_range.nodes).await?;
                    for (node_pos, node_item) in node_items.iter().enumerate() {
                        let node_index = node_range.nodes.start + node_pos;
                        if node_index >= node_range.search_end {
                            continue;
                        }

                        if !bounds.intersects(node_item) {
                            continue;
                        }

                        if node_range.level == 0 {
                            let include_in_page = total_count >= offset && items.len() < page_limit;
                            total_count = total_count.checked_add(1).ok_or_else(|| {
                                JsValue::from_str("spatial result count overflow")
                            })?;

                            if !include_in_page {
                                continue;
                            }

                            let start = feature_begin
                                .checked_add(node_item.offset)
                                .ok_or_else(|| JsValue::from_str("feature range overflow"))?;
                            if let Some(next_node_item) = node_items.get(node_pos + 1) {
                                let end = feature_begin
                                    .checked_add(next_node_item.offset)
                                    .ok_or_else(|| JsValue::from_str("feature range overflow"))?;
                                items.push(HttpSearchResultItem64 {
                                    range: HttpRange64::Range(start..end),
                                });
                            } else {
                                items.push(HttpSearchResultItem64 {
                                    range: HttpRange64::RangeFrom(start..),
                                });
                            }
                        } else {
                            let children_level = node_range.level - 1;
                            let children_start = usize::try_from(node_item.offset)
                                .map_err(|_| JsValue::from_str("R-tree child index overflow"))?;
                            let children_end_u64 = node_item
                                .offset
                                .checked_add(branching_factor as u64)
                                .ok_or_else(|| JsValue::from_str("R-tree child range overflow"))?;
                            let mut children_nodes = children_start
                                ..usize::try_from(children_end_u64).map_err(|_| {
                                    JsValue::from_str("R-tree child index overflow")
                                })?;
                            let mut search_end = children_nodes.end;
                            if children_level == 0 {
                                children_nodes.end =
                                    children_nodes.end.checked_add(1).ok_or_else(|| {
                                        JsValue::from_str("R-tree child range overflow")
                                    })?;
                            }
                            children_nodes.end =
                                children_nodes.end.min(level_bounds[children_level].end);
                            search_end = search_end.min(level_bounds[children_level].end);

                            let children_range = NodeRange {
                                nodes: children_nodes,
                                search_end,
                                level: children_level,
                            };

                            let Some(tail) = queue.back_mut() else {
                                queue.push_back(children_range);
                                continue;
                            };

                            if tail.level != children_level {
                                queue.push_back(children_range);
                                continue;
                            }

                            let wasted_bytes = if children_range.nodes.start >= tail.nodes.end {
                                (children_range.nodes.start - tail.nodes.end)
                                    * std::mem::size_of::<NodeItem>()
                            } else {
                                0
                            };
                            if wasted_bytes > combine_request_threshold {
                                queue.push_back(children_range);
                                continue;
                            }

                            tail.nodes.end = children_range.nodes.end;
                            tail.search_end = tail.search_end.max(children_range.search_end);
                        }
                    }
                }

                Ok(SpatialSearchPage64 { total_count, items })
            }
            SpatialQuery::PointIntersects(_, _) | SpatialQuery::PointNearest(_, _) => {
                Err(JsValue::from_str(
                    "Only bbox spatial queries are supported by the u64 WASM range reader.",
                ))
            }
        }
    }

    async fn read_http_node_items64(
        client: &mut WasmBufferedHttpRangeClient64,
        base: u64,
        node_ids: &Range<usize>,
    ) -> Result<Vec<NodeItem>, JsValue> {
        let item_size = std::mem::size_of::<NodeItem>();
        let begin = base
            .checked_add((node_ids.start * item_size) as u64)
            .ok_or_else(|| JsValue::from_str("R-tree node range overflow"))?;
        let length = node_ids
            .len()
            .checked_mul(item_size)
            .ok_or_else(|| JsValue::from_str("R-tree node request too large"))?;
        let bytes = client
            .min_req_size(0)
            .get_range(begin, length)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        let mut node_items = Vec::with_capacity(node_ids.len());
        for node_item_bytes in bytes.chunks(item_size) {
            node_items.push(
                NodeItem::from_reader(&mut std::io::Cursor::new(node_item_bytes))
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
        }
        Ok(node_items)
    }

    fn generate_level_bounds(num_items: usize, node_size: u16) -> Vec<Range<usize>> {
        let mut level_num_nodes = Vec::new();
        let mut n = num_items;
        let mut num_nodes = n;
        level_num_nodes.push(n);
        loop {
            n = n.div_ceil(node_size as usize);
            num_nodes += n;
            level_num_nodes.push(n);
            if n == 1 {
                break;
            }
        }

        let mut level_offsets = Vec::with_capacity(level_num_nodes.len());
        n = num_nodes;
        for size in &level_num_nodes {
            level_offsets.push(n - size);
            n -= size;
        }

        let mut level_bounds = Vec::with_capacity(level_num_nodes.len());
        for i in 0..level_num_nodes.len() {
            level_bounds.push(level_offsets[i]..level_offsets[i] + level_num_nodes[i]);
        }
        level_bounds
    }

    enum FeatureSelection {
        SelectAll(SelectAll),
        SelectSpatial(SelectSpatial),
        SelectAttr(SelectAttr),
    }

    impl FeatureSelection {
        async fn next_feature_buffer<T: AsyncHttpRangeClient>(
            &mut self,
            client: &mut AsyncBufferedHttpRangeClient<T>,
            client64: &mut WasmBufferedHttpRangeClient64,
        ) -> Result<Option<Bytes>, JsValue> {
            match self {
                FeatureSelection::SelectAll(select_all) => select_all.next_buffer(client64).await,
                FeatureSelection::SelectSpatial(select_spatial) => {
                    select_spatial.next_buffer(client64).await
                }
                FeatureSelection::SelectAttr(select_attr) => select_attr.next_buffer(client).await,
            }
        }
    }

    struct SelectAll {
        /// Features left
        features_left: u64,

        /// How many bytes into the file we've read so far
        pos: u64,
    }

    impl SelectAll {
        async fn next_buffer(
            &mut self,
            client: &mut WasmBufferedHttpRangeClient64,
        ) -> Result<Option<Bytes>, JsValue> {
            client.min_req_size(DEFAULT_HTTP_FETCH_SIZE);

            if self.features_left == 0 {
                return Ok(None);
            }
            self.features_left -= 1;

            let mut feature_buffer = BytesMut::from(
                client
                    .get_range(self.pos, 4)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            self.pos += 4;
            let feature_size = LittleEndian::read_u32(&feature_buffer) as usize;
            feature_buffer.put(
                client
                    .get_range(self.pos, feature_size)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            self.pos += feature_size as u64;

            Ok(Some(feature_buffer.freeze()))
        }
    }

    struct SelectSpatial {
        /// Selected features
        feature_batches: Vec<FeatureBatch>,
    }

    impl SelectSpatial {
        async fn next_buffer(
            &mut self,
            client: &mut WasmBufferedHttpRangeClient64,
        ) -> Result<Option<Bytes>, JsValue> {
            let mut next_buffer = None;
            while next_buffer.is_none() {
                let Some(feature_batch) = self.feature_batches.last_mut() else {
                    break;
                };
                let Some(buffer) = feature_batch.next_buffer(client).await? else {
                    // done with this batch
                    self.feature_batches
                        .pop()
                        .expect("already asserted feature_batches was non-empty");
                    continue;
                };
                next_buffer = Some(buffer)
            }

            Ok(next_buffer)
        }
    }

    struct FeatureBatch {
        /// The byte location of each feature within the file
        feature_ranges: VecDeque<HttpRange64>,
    }

    impl FeatureBatch {
        async fn make_batches(
            feature_ranges: Vec<HttpSearchResultItem64>,
            combine_request_threshold: usize,
        ) -> Result<Vec<Self>, JsValue> {
            let mut batched_ranges = vec![];

            for search_result_item in feature_ranges.into_iter() {
                let Some(latest_batch) = batched_ranges.last_mut() else {
                    let mut new_batch = VecDeque::new();
                    new_batch.push_back(search_result_item.range);
                    batched_ranges.push(new_batch);
                    continue;
                };

                let previous_item = latest_batch.back().expect("we never push an empty batch");

                let HttpRange64::Range(Range { end: prev_end, .. }) = previous_item else {
                    debug_assert!(false, "This shouldn't happen. Only the very last feature is expected to have an unknown length");
                    let mut new_batch = VecDeque::new();
                    new_batch.push_back(search_result_item.range);
                    batched_ranges.push(new_batch);
                    continue;
                };

                let wasted_bytes = search_result_item.range.start().saturating_sub(*prev_end);
                if wasted_bytes < combine_request_threshold as u64 {
                    latest_batch.push_back(search_result_item.range)
                } else {
                    let mut new_batch = VecDeque::new();
                    new_batch.push_back(search_result_item.range);
                    batched_ranges.push(new_batch);
                }
            }

            let mut batches: Vec<_> = batched_ranges.into_iter().map(FeatureBatch::new).collect();
            batches.reverse();
            Ok(batches)
        }

        fn new(feature_ranges: VecDeque<HttpRange64>) -> Self {
            Self { feature_ranges }
        }

        /// When fetching new data, how many bytes should we fetch at once.
        /// It was computed based on the specific feature ranges of the batch
        /// to optimize number of requests vs. wasted bytes vs. resident memory
        fn request_size(&self) -> usize {
            let Some(first) = self.feature_ranges.front() else {
                return 0;
            };
            let Some(last) = self.feature_ranges.back() else {
                return 0;
            };

            // `last.length()` should only be None if this batch includes the final feature
            // in the dataset. Since we can't infer its actual length, we'll fetch only
            // the first 4 bytes of that feature buffer, which will tell us the actual length
            // of the feature buffer for the subsequent request.
            let last_feature_length = last.length().unwrap_or(4);

            let covering_len = last
                .start()
                .saturating_add(last_feature_length)
                .saturating_sub(first.start());

            usize::try_from(covering_len)
                .unwrap_or(usize::MAX)
                // Since it's all held in memory, don't fetch more than DEFAULT_HTTP_FETCH_SIZE at a time
                // unless necessary.
                .min(DEFAULT_HTTP_FETCH_SIZE)
        }

        async fn next_buffer(
            &mut self,
            client: &mut WasmBufferedHttpRangeClient64,
        ) -> Result<Option<Bytes>, JsValue> {
            let request_size = self.request_size();
            client.set_min_req_size(request_size);
            let Some(feature_range) = self.feature_ranges.pop_front() else {
                return Ok(None);
            };

            let mut pos = feature_range.start();
            let mut feature_buffer = BytesMut::from(
                client
                    .get_range(pos, 4)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            pos += 4;
            let feature_size = LittleEndian::read_u32(&feature_buffer) as usize;
            feature_buffer.put(
                client
                    .get_range(pos, feature_size)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );

            Ok(Some(feature_buffer.freeze()))
        }
    }

    struct SelectAttr {
        // TODO: change this implementation so it can batch features
        ranges: Vec<HttpRange>,
        range_pos: usize,
    }

    impl SelectAttr {
        async fn next_buffer<T: AsyncHttpRangeClient>(
            &mut self,
            client: &mut AsyncBufferedHttpRangeClient<T>,
        ) -> Result<Option<Bytes>, JsValue> {
            let Some(range) = self.ranges.get(self.range_pos) else {
                return Ok(None);
            };
            let mut feature_buffer = BytesMut::from(
                client
                    .get_range(range.start(), 4)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            let feature_size = LittleEndian::read_u32(&feature_buffer) as usize;
            feature_buffer.put(
                client
                    .get_range(range.start() + 4, feature_size)
                    .await
                    .map_err(|e| JsValue::from_str(&e.to_string()))?,
            );
            self.range_pos += 1;
            Ok(Some(feature_buffer.freeze()))
        }
    }

    #[wasm_bindgen]
    pub struct WasmSpatialQuery {
        // Making inner private to hide it from WASM - don't expose SpatialQuery type
        #[wasm_bindgen(skip)]
        inner: SpatialQuery,
    }

    #[wasm_bindgen]
    impl WasmSpatialQuery {
        #[wasm_bindgen(constructor)]
        pub fn new(js_value: &JsValue) -> Result<WasmSpatialQuery, JsValue> {
            // Parse the JS object to extract query parameters
            let obj = js_sys::Object::from(js_value.clone());

            // Get the query type
            let query_type = js_sys::Reflect::get(&obj, &JsValue::from_str("type"))
                .map_err(|_| JsValue::from_str("Missing 'type' field in query object"))?
                .as_string()
                .ok_or_else(|| JsValue::from_str("Query type must be a string"))?;

            // Build the appropriate query based on type
            let query = match query_type.as_str() {
                "bbox" => {
                    // Extract bbox coordinates
                    let min_x = get_number_property(&obj, "minX")?;
                    let min_y = get_number_property(&obj, "minY")?;
                    let max_x = get_number_property(&obj, "maxX")?;
                    let max_y = get_number_property(&obj, "maxY")?;

                    fcb_core::packed_rtree::Query::BBox(min_x, min_y, max_x, max_y)
                }
                "pointIntersects" => {
                    // Extract point coordinates
                    let x = get_number_property(&obj, "x")?;
                    let y = get_number_property(&obj, "y")?;

                    fcb_core::packed_rtree::Query::PointIntersects(x, y)
                }
                "pointNearest" => {
                    // Extract point coordinates
                    let x = get_number_property(&obj, "x")?;
                    let y = get_number_property(&obj, "y")?;

                    fcb_core::packed_rtree::Query::PointNearest(x, y)
                }
                _ => {
                    return Err(JsValue::from_str(&format!(
                        "Unsupported query type: {query_type}"
                    )))
                }
            };

            Ok(WasmSpatialQuery { inner: query })
        }

        // Instead of exposing inner directly, expose its components via getter methods
        #[wasm_bindgen(getter)]
        pub fn query_type(&self) -> String {
            match self.inner {
                SpatialQuery::BBox(_, _, _, _) => "bbox".to_string(),
                SpatialQuery::PointIntersects(_, _) => "pointIntersects".to_string(),
                SpatialQuery::PointNearest(_, _) => "pointNearest".to_string(),
            }
        }

        #[wasm_bindgen(getter)]
        pub fn min_x(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::BBox(min_x, _, _, _) => Some(min_x),
                _ => None,
            }
        }

        #[wasm_bindgen(getter)]
        pub fn min_y(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::BBox(_, min_y, _, _) => Some(min_y),
                _ => None,
            }
        }

        #[wasm_bindgen(getter)]
        pub fn max_x(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::BBox(_, _, max_x, _) => Some(max_x),
                _ => None,
            }
        }

        #[wasm_bindgen(getter)]
        pub fn max_y(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::BBox(_, _, _, max_y) => Some(max_y),
                _ => None,
            }
        }

        #[wasm_bindgen(getter)]
        pub fn x(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::PointIntersects(x, _) | SpatialQuery::PointNearest(x, _) => Some(x),
                _ => None,
            }
        }

        #[wasm_bindgen(getter)]
        pub fn y(&self) -> Option<f64> {
            match self.inner {
                SpatialQuery::PointIntersects(_, y) | SpatialQuery::PointNearest(_, y) => Some(y),
                _ => None,
            }
        }

        // Method to get the query data in a JS-friendly format
        pub fn to_js(&self) -> JsValue {
            match self.inner {
                SpatialQuery::BBox(min_x, min_y, max_x, max_y) => {
                    let obj = js_sys::Object::new();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("type"),
                        &JsValue::from_str("bbox"),
                    )
                    .unwrap();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("minX"),
                        &JsValue::from_f64(min_x),
                    )
                    .unwrap();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("minY"),
                        &JsValue::from_f64(min_y),
                    )
                    .unwrap();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("maxX"),
                        &JsValue::from_f64(max_x),
                    )
                    .unwrap();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("maxY"),
                        &JsValue::from_f64(max_y),
                    )
                    .unwrap();
                    obj.into()
                }
                SpatialQuery::PointIntersects(x, y) => {
                    let obj = js_sys::Object::new();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("type"),
                        &JsValue::from_str("pointIntersects"),
                    )
                    .unwrap();
                    js_sys::Reflect::set(&obj, &JsValue::from_str("x"), &JsValue::from_f64(x))
                        .unwrap();
                    js_sys::Reflect::set(&obj, &JsValue::from_str("y"), &JsValue::from_f64(y))
                        .unwrap();
                    obj.into()
                }
                SpatialQuery::PointNearest(x, y) => {
                    let obj = js_sys::Object::new();
                    js_sys::Reflect::set(
                        &obj,
                        &JsValue::from_str("type"),
                        &JsValue::from_str("pointNearest"),
                    )
                    .unwrap();
                    js_sys::Reflect::set(&obj, &JsValue::from_str("x"), &JsValue::from_f64(x))
                        .unwrap();
                    js_sys::Reflect::set(&obj, &JsValue::from_str("y"), &JsValue::from_f64(y))
                        .unwrap();
                    obj.into()
                }
            }
        }

        // Internal helper for other methods to access the inner query
        fn get_inner(&self) -> SpatialQuery {
            match self.inner {
                SpatialQuery::BBox(min_x, min_y, max_x, max_y) => {
                    SpatialQuery::BBox(min_x, min_y, max_x, max_y)
                }
                SpatialQuery::PointIntersects(x, y) => SpatialQuery::PointIntersects(x, y),
                SpatialQuery::PointNearest(x, y) => SpatialQuery::PointNearest(x, y),
            }
        }
    }

    // Helper function to extract number properties from JS objects
    fn get_number_property(obj: &js_sys::Object, property: &str) -> Result<f64, JsValue> {
        let property_value = js_sys::Reflect::get(obj, &JsValue::from_str(property))
            .map_err(|_| JsValue::from_str(&format!("Missing '{property}' field")))?;

        property_value
            .as_f64()
            .ok_or_else(|| JsValue::from_str(&format!("'{property}' must be a number")))
    }

    /// A wasm‑friendly wrapper over `AttrQuery`, which is defined as:
    /// `pub type AttrQuery = Vec<(String, Operator, )>;`
    #[wasm_bindgen]
    #[derive(Debug)]
    pub struct WasmAttrQuery {
        inner: AttrQuery,
    }

    #[wasm_bindgen]
    impl WasmAttrQuery {
        /// Creates a new WasmAttrQuery from a JS array of query tuples.
        ///
        /// Each query tuple must be an array of three elements:
        /// [field: string, operator: string, value: number | boolean | string | Date]
        ///
        /// For example, in JavaScript you could pass:
        /// `[ ["b3_h_dak_50p", "Gt", 2.0],
        ///   ["identificatie", "Eq", "NL.IMBAG.Pand.0503100000012869"],
        ///   ["created", "Ge", new Date("2020-01-01T00:00:00Z")] ]`
        #[wasm_bindgen(constructor)]
        pub fn new(js_value: &JsValue) -> Result<WasmAttrQuery, JsValue> {
            // Expect the JS value to be an array of query tuples.
            let arr = Array::from(js_value);
            let mut inner: AttrQuery = Vec::new();

            for tuple in arr.iter() {
                // Each tuple is expected to be an array with at least 3 elements.
                let tuple_arr = Array::from(&tuple);
                if tuple_arr.length() < 3 {
                    return Err(JsValue::from_str("Each query tuple must have 3 elements"));
                }

                // First element: field name (string)
                let field = tuple_arr
                    .get(0)
                    .as_string()
                    .ok_or_else(|| JsValue::from_str("Field must be a string"))?;

                // Second element: operator as string, converting to the Operator enum.
                let op_str = tuple_arr
                    .get(1)
                    .as_string()
                    .ok_or_else(|| JsValue::from_str("Operator must be a string"))?;
                let operator = match op_str.as_str() {
                    "Eq" => Operator::Eq,
                    "Gt" => Operator::Gt,
                    "Ge" => Operator::Ge,
                    "Lt" => Operator::Lt,
                    "Le" => Operator::Le,
                    "Ne" => Operator::Ne,
                    _ => return Err(JsValue::from_str("Invalid operator value")),
                };

                // Third element: the value
                let value_js = tuple_arr.get(2);
                let bs_value = if let Some(b) = value_js.as_bool() {
                    // If boolean then use Bool
                    KeyType::Bool(b)
                } else if value_js.is_instance_of::<js_sys::Date>() {
                    // If a JS Date, convert to milliseconds then to a NaiveDateTime.
                    let date: js_sys::Date = value_js.unchecked_into();
                    let millis = date.get_time();
                    let secs = (millis / 1000.0) as i64;
                    let nanos = ((millis % 1000.0) * 1_000_000.0) as u32;
                    let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
                        .expect("invalid datetime value");
                    KeyType::DateTime(dt)
                } else if let Some(n) = value_js.as_f64() {
                    // All JS numbers are f64.
                    KeyType::Float64(Float(n))
                } else if let Some(s) = value_js.as_string() {
                    if s.len() > 50 {
                        KeyType::StringKey100(FixedStringKey::<100>::from_str(&s))
                    } else {
                        KeyType::StringKey50(FixedStringKey::<50>::from_str(&s))
                    }
                } else {
                    return Err(JsValue::from_str("Unsupported value type in query tuple"));
                };

                inner.push((field, operator, bs_value));
            }

            Ok(WasmAttrQuery { inner })
        }

        /// Returns the inner AttrQuery as a JsValue (an array of query tuples)
        /// useful for debugging.
        #[wasm_bindgen(getter)]
        pub fn inner(&self) -> JsValue {
            let arr = Array::new();
            for (field, op, val) in self.inner.iter() {
                let tuple = Array::new();
                tuple.push(&JsValue::from_str(field));
                let op_str = match op {
                    Operator::Eq => "Eq",
                    Operator::Gt => "Gt",
                    Operator::Ge => "Ge",
                    Operator::Lt => "Lt",
                    Operator::Le => "Le",
                    Operator::Ne => "Ne",
                };
                tuple.push(&JsValue::from_str(op_str));
                let val_js = match val {
                    KeyType::Int8(n) => JsValue::from_f64(*n as f64),
                    KeyType::UInt8(n) => JsValue::from_f64(*n as f64),
                    KeyType::Int16(n) => JsValue::from_f64(*n as f64),
                    KeyType::UInt16(n) => JsValue::from_f64(*n as f64),
                    KeyType::Int64(n) => JsValue::from_f64(*n as f64),
                    KeyType::Int32(n) => JsValue::from_f64(*n as f64),
                    KeyType::UInt64(n) => JsValue::from_f64(*n as f64),
                    KeyType::UInt32(n) => JsValue::from_f64(*n as f64),
                    KeyType::Float64(f) => JsValue::from_f64(f.into_inner()),
                    KeyType::Float32(f) => JsValue::from_f64(f.into_inner() as f64),
                    KeyType::Bool(b) => JsValue::from_bool(*b),
                    KeyType::StringKey100(s) => JsValue::from_str(&s.to_string_lossy()),
                    KeyType::StringKey50(s) => JsValue::from_str(&s.to_string_lossy()),
                    KeyType::StringKey20(s) => JsValue::from_str(&s.to_string_lossy()),
                    KeyType::DateTime(dt) => JsValue::from_str(&dt.to_rfc3339()),
                };
                tuple.push(&val_js);
                arr.push(&tuple);
            }
            arr.into()
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use util::convert_cityjson_to_obj;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;
