/*
 *
 *  * Copyright (c) 2025 Couchbase, Inc.
 *  *
 *  * Licensed under the Apache License, Version 2.0 (the "License");
 *  * you may not use this file except in compliance with the License.
 *  * You may obtain a copy of the License at
 *  *
 *  *    http://www.apache.org/licenses/LICENSE-2.0
 *  *
 *  * Unless required by applicable law or agreed to in writing, software
 *  * distributed under the License is distributed on an "AS IS" BASIS,
 *  * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  * See the License for the specific language governing permissions and
 *  * limitations under the License.
 *
 */

use std::collections::HashMap;

use crate::cbconfig::{TerseConfig, TerseExtNodePorts, VBucketServerMap};
use crate::clusterlabels::ClusterLabels;
use crate::error;
use crate::error::{ErrorKind, Result};
use crate::parsedconfig::{
    BucketType, ParsedConfig, ParsedConfigBucket, ParsedConfigBucketFeature, ParsedConfigFeature,
    ParsedConfigNode, ParsedConfigNodeAddresses, ParsedConfigNodePorts,
};
use crate::vbucketmap::VbucketMap;

pub(crate) struct ConfigParser {}

impl ConfigParser {
    pub fn parse_terse_config(config: TerseConfig, source_hostname: &str) -> Result<ParsedConfig> {
        let rev_id = config.rev;
        let rev_epoch = config.rev_epoch.unwrap_or_default();

        let len_nodes = if let Some(nodes) = config.nodes {
            nodes.len()
        } else {
            0
        };
        let mut nodes = Vec::with_capacity(config.nodes_ext.len());
        for (node_idx, node) in config.nodes_ext.into_iter().enumerate() {
            let node_hostname = Self::parse_config_hostname(&node.hostname, source_hostname);

            let mut alt_addresses = HashMap::new();
            for (network_type, alt_addrs) in node.alternate_addresses {
                let alt_hostname = Self::parse_config_hostname(&alt_addrs.hostname, &node_hostname);
                let this_address = Self::parse_config_hosts_into(&alt_hostname, alt_addrs.ports);

                alt_addresses.insert(network_type, this_address);
            }

            let this_node = ParsedConfigNode {
                has_data: node_idx < len_nodes,
                addresses: Self::parse_config_hosts_into(
                    &node_hostname,
                    node.services.unwrap_or_default(),
                ),
                alt_addresses,
            };

            nodes.push(this_node);
        }

        let bucket = if let Some(bucket_name) = config.name {
            let bucket_uuid = config.uuid.unwrap_or_default();
            let (bucket_type, vbucket_map) = if let Some(locator) = config.node_locator {
                match locator.as_str() {
                    "vbucket" => (
                        BucketType::Couchbase,
                        Self::parse_vbucket_server_map(config.vbucket_server_map)?,
                    ),
                    _ => (BucketType::Invalid, None),
                }
            } else {
                (BucketType::Invalid, None)
            };

            let mut features = vec![];
            if let Some(bucket_capabilities) = config.bucket_capabilities {
                for cap in bucket_capabilities {
                    let feat = ParsedConfigBucketFeature::from(cap);
                    if feat != ParsedConfigBucketFeature::Unknown {
                        features.push(feat);
                    }
                }
            }

            Some(ParsedConfigBucket {
                bucket_uuid,
                bucket_name,
                bucket_type,
                vbucket_map,
                features,
            })
        } else {
            None
        };

        let mut features = vec![];
        if let Some(caps) = config.cluster_capabilities.get("search") {
            if caps.contains(&"vectorSearch".to_string()) {
                features.push(ParsedConfigFeature::FtsVectorSearch);
            }
        }

        let cluster_labels = if config.cluster_name.is_some() || config.cluster_uuid.is_some() {
            Some(ClusterLabels {
                cluster_name: config.cluster_name,
                cluster_uuid: config.cluster_uuid,
            })
        } else {
            None
        };

        Ok(ParsedConfig {
            rev_id,
            rev_epoch,
            bucket,
            nodes,
            features,
            source_hostname: source_hostname.to_string(),
            cluster_labels,
        })
    }

    fn parse_config_hostname(hostname: &Option<String>, source_hostname: &str) -> String {
        if let Some(hostname) = hostname {
            if hostname.contains(':') {
                return format!("[{hostname}]");
            }

            hostname.to_string()
        } else {
            source_hostname.to_string()
        }
    }

    fn parse_config_hosts_into(
        hostname: &str,
        ports: TerseExtNodePorts,
    ) -> ParsedConfigNodeAddresses {
        ParsedConfigNodeAddresses {
            hostname: hostname.to_string(),
            non_ssl_ports: ParsedConfigNodePorts {
                kv: ports.kv,
                mgmt: ports.mgmt,
                analytics: ports.cbas,
                query: ports.n1ql,
                search: ports.fts,
                index_http: ports.gsi,
                index_scan: ports.index_scan,
            },
            ssl_ports: ParsedConfigNodePorts {
                kv: ports.kv_ssl,
                mgmt: ports.mgmt_ssl,
                analytics: ports.cbas_ssl,
                query: ports.n1ql_ssl,
                search: ports.fts_ssl,
                index_http: ports.gsi_ssl,
                // Repeated rather than absent: the server advertises no
                // `indexScanSSL`, so the TLS queryport is this port with TLS on
                // top of it. A `None` here would make a TLS-only cluster's
                // indexer unreachable.
                index_scan: ports.index_scan,
            },
        }
    }

    fn parse_vbucket_server_map(
        vbucket_server_map: Option<VBucketServerMap>,
    ) -> Result<Option<VbucketMap>> {
        if let Some(vbucket_server_map) = vbucket_server_map {
            if vbucket_server_map.vbucket_map.is_empty() {
                return Err(error::Error::from(ErrorKind::NoVbucketMap));
            }

            return Ok(Some(VbucketMap::new(
                vbucket_server_map.vbucket_map,
                vbucket_server_map.num_replicas,
            )?));
        }

        Ok(None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cbconfig::parse::parse_terse_config;

    /// Captured verbatim from `/pools/default/b/default` on a four-node 8.0.3
    /// cluster — three data/index/query nodes and one search-only node. Trimmed
    /// only in the vbucket map and the `nodes` array; every `services` object is
    /// exactly what the server sent, because the field names are the contract
    /// and paraphrasing them would test the paraphrase.
    ///
    /// The thing to notice is what is *not* here: `indexScan`, `indexHttp` and
    /// `indexHttps` are advertised and there is no `indexScanSSL`.
    pub(crate) const SAMPLE: &str = r#"{
      "rev": 13298683,
      "revEpoch": 1,
      "name": "default",
      "nodeLocator": "vbucket",
      "uuid": "8857a7b9c24e24eba57ab0a94aea7a6e",
      "bucketCapabilities": ["collections", "rangeScan"],
      "nodesExt": [
        {
          "nodeUUID": "ad7d168ff6c4513a7538d670611260dd",
          "services": {
            "capi": 8092,
            "capiSSL": 18092,
            "indexAdmin": 9100,
            "indexHttp": 9102,
            "indexHttps": 19102,
            "indexScan": 9101,
            "indexStreamCatchup": 9104,
            "indexStreamInit": 9103,
            "indexStreamMaint": 9105,
            "kv": 11210,
            "kvSSL": 11207,
            "mgmt": 8091,
            "mgmtSSL": 18091,
            "n1ql": 8093,
            "n1qlSSL": 18093,
            "projector": 9999
          },
          "thisNode": true,
          "hostname": "192.168.107.128",
          "serverGroup": "Group 1"
        },
        {
          "nodeUUID": "e71ce1ef15b7cdd2324c7e75678d854c",
          "services": {
            "fts": 8094,
            "ftsGRPC": 9130,
            "ftsGRPCSSL": 19130,
            "ftsSSL": 18094,
            "mgmt": 8091,
            "mgmtSSL": 18091
          },
          "hostname": "192.168.107.131",
          "serverGroup": "Group 1"
        }
      ],
      "clusterCapabilitiesVer": [1, 0],
      "clusterCapabilities": { "search": ["vectorSearch", "scopedSearchIndex"] },
      "clusterUUID": "a3a12a23906a89f44bd3b150b2a647b5",
      "clusterName": "test-cluster",
      "vBucketServerMap": {
        "hashAlgorithm": "CRC",
        "numReplicas": 1,
        "serverList": ["192.168.107.128:11210"],
        "vBucketMap": [[0, -1], [0, -1]]
      },
      "nodes": [{ "hostname": "192.168.107.128:8091", "ports": { "direct": 11210 } }]
    }"#;

    /// Shared with [`crate::indexrouter`]'s tests, which join against the same
    /// capture rather than against a hand-written config that could agree with
    /// the router and disagree with the server.
    pub(crate) fn parse_sample() -> ParsedConfig {
        let terse = parse_terse_config(SAMPLE.as_bytes(), "127.0.0.1").expect("the capture parses");
        ConfigParser::parse_terse_config(terse, "127.0.0.1").expect("the capture is a valid config")
    }

    #[test]
    fn both_index_ports_are_read_and_are_not_the_same_port() {
        // The blocker this test exists for: before `index_scan` was parsed, the
        // only index port a config carried was `indexHttp`, and a scan endpoint
        // could not be resolved at all. The two ports differ, so using one for
        // the other reaches a service that speaks a different protocol.
        let config = parse_sample();
        let ports = &config.nodes[0].addresses.non_ssl_ports;

        assert_eq!(ports.index_scan, Some(9101));
        assert_eq!(ports.index_http, Some(9102));
    }

    #[test]
    fn the_ssl_set_takes_index_https_but_repeats_the_scan_port() {
        // `indexHttps` is a distinct port and is read as one; there is no
        // `indexScanSSL` in the capture, so the TLS queryport is 9101 with TLS
        // on top. Leaving it `None` would make a TLS-only cluster's indexer
        // unreachable, which is exactly the bug this arm prevents.
        let config = parse_sample();
        let ssl = &config.nodes[0].addresses.ssl_ports;

        assert_eq!(ssl.index_http, Some(19102));
        assert_eq!(ssl.index_scan, Some(9101));
        assert_eq!(
            ssl.index_scan,
            config.nodes[0].addresses.non_ssl_ports.index_scan
        );
    }

    #[test]
    fn a_node_running_no_indexer_has_no_index_ports() {
        // The search-only node in the capture. `Option` rather than `0` is what
        // lets a router skip it instead of dialling port zero.
        let config = parse_sample();
        let node = &config.nodes[1];

        assert_eq!(node.addresses.hostname, "192.168.107.131");
        assert_eq!(node.addresses.non_ssl_ports.index_scan, None);
        assert_eq!(node.addresses.non_ssl_ports.index_http, None);
        assert_eq!(node.addresses.ssl_ports.index_scan, None);
        assert_eq!(node.addresses.ssl_ports.index_http, None);
        assert_eq!(node.addresses.non_ssl_ports.search, Some(8094));
    }

    #[test]
    fn the_index_ports_survive_the_network_type_projection() {
        // `addresses_group_for_network_type` is what every component actually
        // reads, and it copies port structs field by field; a field added to
        // `ParsedConfigNodePorts` and missed there would be silently dropped.
        let config = parse_sample();
        let network = config.addresses_group_for_network_type("default");

        assert_eq!(network.nodes[0].non_ssl_ports.index_scan, Some(9101));
        assert_eq!(network.nodes[0].ssl_ports.index_http, Some(19102));
        assert_eq!(
            network.nodes[0]
                .canonical_node_info
                .non_ssl_ports
                .index_scan,
            Some(9101)
        );
    }
}
