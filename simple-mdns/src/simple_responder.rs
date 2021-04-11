use std::{
    convert::TryInto,
    net::IpAddr,
    sync::{Arc, RwLock},
};

use simple_dns::{
    rdata::{RData, A, AAAA, SRV},
    PacketBuf, PacketHeader, ResourceRecord, CLASS, QTYPE, TYPE,
};

use crate::{
    create_udp_socket, resource_record_manager::ResourceRecordManager, SimpleMdnsError,
    ENABLE_LOOPBACK, MULTICAST_ADDR_IPV4, MULTICAST_PORT,
};

const FIVE_MINUTES: u32 = 60 * 5;

/// A simple mDNS responder aimed for service discovery.  
/// This struct is provided as an alternative for external mDNS resource configuration
pub struct SimpleMdnsResponder {
    enable_loopback: bool,
    resources: Arc<RwLock<ResourceRecordManager<'static>>>,
    rr_ttl: u32,
}

impl SimpleMdnsResponder {
    /// Creates a new SimpleMdnsResponder with ttl of 5 minutes and enabled loopback
    pub fn new(rr_ttl: u32, enable_loopback: bool) -> Self {
        Self {
            resources: Arc::new(RwLock::new(ResourceRecordManager::new())),
            enable_loopback,
            rr_ttl,
        }
    }

    /// Register a Resource Record
    pub fn add_resource(&mut self, resource: ResourceRecord<'static>) {
        let mut resources = self.resources.write().unwrap();
        resources.add_resource(resource);
    }

    /// Remove a resource record by service name and resource type
    pub fn remove_resource_record(&mut self, service_name: &str, resource_type: TYPE) {
        let mut resources = self.resources.write().unwrap();
        resources.remove_resource_record(service_name, &resource_type);
    }

    /// Remove all resource records for a service name
    pub fn remove_all_resource_records(&mut self, service_name: &str) {
        let mut resources = self.resources.write().unwrap();
        resources.remove_all_resource_records(service_name);
    }

    /// Register the service address as a Resource Record
    pub fn add_service_address(
        &mut self,
        name: &'static str,
        addr: IpAddr,
        port: u16,
    ) -> Result<(), crate::SimpleMdnsError> {
        let addr_resource = match addr {
            IpAddr::V4(ip) => ResourceRecord::new(
                name,
                TYPE::A,
                CLASS::IN,
                self.rr_ttl,
                RData::A(A { address: ip.into() }),
            ),
            IpAddr::V6(ip) => ResourceRecord::new(
                name,
                TYPE::AAAA,
                CLASS::IN,
                self.rr_ttl,
                RData::AAAA(AAAA { address: ip.into() }),
            ),
        }?;

        self.add_resource(addr_resource);
        self.add_resource(ResourceRecord::new(
            name,
            TYPE::SRV,
            CLASS::IN,
            self.rr_ttl,
            RData::SRV(Box::new(SRV {
                port,
                priority: 0,
                target: name.try_into()?,
                weight: 0,
            })),
        )?);

        Ok(())
    }

    /// Start listening to requests
    pub fn listen(&self) {
        let enable_loopback = self.enable_loopback;
        let resources = self.resources.clone();
        tokio::spawn(async move {
            Self::create_socket_and_wait_messages(enable_loopback, resources).await
        });
    }

    async fn create_socket_and_wait_messages(
        enable_loopback: bool,
        resources: Arc<RwLock<ResourceRecordManager<'static>>>,
    ) -> Result<(), SimpleMdnsError> {
        let mut recv_buffer = vec![0; 4096];

        let socket = create_udp_socket(enable_loopback)
            .map_err(|_| SimpleMdnsError::ErrorCreatingUDPSocket)?;

        loop {
            let (count, addr) = socket
                .recv_from(&mut recv_buffer)
                .await
                .map_err(|_| SimpleMdnsError::ErrorReadingFromUDPSocket)?;

            let packet = PacketBuf::from(&recv_buffer[..count]);
            let response = build_reply(packet, &resources.read().unwrap());
            if let Some((unicast_response, reply_packet)) = response {
                let target_addr = if unicast_response {
                    addr
                } else {
                    std::net::SocketAddr::new(MULTICAST_ADDR_IPV4.into(), MULTICAST_PORT)
                };

                socket
                    .send_to(&reply_packet, target_addr)
                    .await
                    .map_err(|_| SimpleMdnsError::ErrorSendingDNSPacket)?;
            }
        }
    }

    /// Set the simple mdns responder's enable loopback.
    pub fn set_enable_loopback(&mut self, enable_loopback: bool) {
        self.enable_loopback = enable_loopback;
    }

    /// Set the simple mdns responder's rr default ttl in seconds (defaults to 300).
    pub fn set_rr_ttl(&mut self, rr_default_ttl: u32) {
        self.rr_ttl = rr_default_ttl;
    }
}

impl Default for SimpleMdnsResponder {
    fn default() -> Self {
        Self::new(FIVE_MINUTES, ENABLE_LOOPBACK)
    }
}

fn build_reply(packet: PacketBuf, resources: &ResourceRecordManager) -> Option<(bool, PacketBuf)> {
    let header = PacketHeader::parse(&packet).ok()?;
    let mut reply_packet = PacketBuf::new(PacketHeader::new_reply(header.id, header.opcode));

    let mut unicast_response = false;
    let mut additional_records = Vec::new();
    for question in packet.questions_iter() {
        if question.unicast_response {
            unicast_response = question.unicast_response
        }

        for answer in resources.find_matching_resources(
            &question.qname.to_string(),
            question.qtype,
            question.qclass,
        ) {
            reply_packet.add_answer(answer).ok()?;

            if let RData::SRV(srv) = &answer.rdata {
                additional_records.extend(resources.find_matching_resources(
                    &srv.target.to_string(),
                    QTYPE::A,
                    question.qclass,
                ));
            }
        }
    }

    for additional_record in additional_records {
        reply_packet.add_additional_record(additional_record).ok()?;
    }

    if reply_packet.has_answers() {
        Some((unicast_response, reply_packet))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::TryInto,
        net::{Ipv4Addr, Ipv6Addr},
    };

    use simple_dns::{
        rdata::{A, AAAA, SRV},
        Question, CLASS, QCLASS
    };

    use super::*;

    fn get_resources() -> ResourceRecordManager<'static> {
        let mut resources = ResourceRecordManager::new();
        resources.add_resource(ResourceRecord {
            class: CLASS::IN,
            name: "_res1._tcp.com".try_into().unwrap(),
            rdatatype: simple_dns::TYPE::A,
            ttl: 10,
            rdata: RData::A(A {
                address: Ipv4Addr::LOCALHOST.into(),
            }),
        });
        resources.add_resource(ResourceRecord {
            class: CLASS::IN,
            name: "_res1._tcp.com".try_into().unwrap(),
            rdatatype: simple_dns::TYPE::AAAA,
            ttl: 10,
            rdata: RData::AAAA(AAAA {
                address: Ipv6Addr::LOCALHOST.into(),
            }),
        });
        resources.add_resource(ResourceRecord {
            class: CLASS::IN,
            name: "_res1._tcp.com".try_into().unwrap(),
            rdatatype: simple_dns::TYPE::SRV,
            ttl: 10,
            rdata: RData::SRV(Box::new(SRV {
                port: 8080,
                priority: 0,
                weight: 0,
                target: "_res1._tcp.com".try_into().unwrap(),
            })),
        });

        resources.add_resource(ResourceRecord {
            class: CLASS::IN,
            name: "_res2._tcp.com".try_into().unwrap(),
            rdatatype: simple_dns::TYPE::A,
            ttl: 10,
            rdata: RData::A(A {
                address: Ipv4Addr::LOCALHOST.into(),
            }),
        });

        resources
    }


    #[test]
    fn test_add_service_address() {
        let mut responder = SimpleMdnsResponder::default();
        responder.add_service_address("_res1._tcp.com", IpAddr::V4(Ipv4Addr::LOCALHOST), 8080).unwrap();
        responder.add_service_address("_res1._tcp.com", IpAddr::V6(Ipv6Addr::LOCALHOST), 8080).unwrap();
        responder.add_service_address("_res2._tcp.com", IpAddr::V4(Ipv4Addr::LOCALHOST), 8080).unwrap();

        let resources = responder.resources.read().unwrap();

        assert_eq!(3, resources.find_matching_resources("_res1._tcp.com", QTYPE::ANY, QCLASS::IN).count());
        assert_eq!(1, resources.find_matching_resources("_res1._tcp.com", QTYPE::SRV, QCLASS::IN).count());
        assert_eq!(2, resources.find_matching_resources("_res2._tcp.com", QTYPE::ANY, QCLASS::ANY).count());
    }


    #[test]
    fn test_build_reply_with_no_questions() {
        let resources = get_resources();

        let packet = PacketBuf::new(PacketHeader::new_query(1, false));
        assert!(build_reply(packet, &resources).is_none());
    }

    #[test]
    fn test_build_reply_without_valid_answers() {
        let resources = get_resources();

        let mut packet = PacketBuf::new(PacketHeader::new_query(1, false));
        packet
            .add_question(&Question::new(
                "_res3._tcp.com".try_into().unwrap(),
                simple_dns::QTYPE::ANY,
                simple_dns::QCLASS::ANY,
                false,
            ))
            .unwrap();

        assert!(build_reply(packet, &resources).is_none());
    }

    #[test]
    fn test_build_reply_with_valid_answer() {
        let resources = get_resources();

        let mut packet = PacketBuf::new(PacketHeader::new_query(1, false));
        packet
            .add_question(&Question::new(
                "_res1._tcp.com".try_into().unwrap(),
                simple_dns::QTYPE::A,
                simple_dns::QCLASS::ANY,
                false,
            ))
            .unwrap();

        let (unicast_response, reply) = build_reply(packet, &resources).unwrap();
        let reply = reply.to_packet().unwrap();

        assert!(!unicast_response);
        assert_eq!(2, reply.answers.len());
        assert_eq!(0, reply.additional_records.len());
    }

    #[test]
    fn test_build_reply_for_srv() {
        let resources = get_resources();

        let mut packet = PacketBuf::new(PacketHeader::new_query(1, false));
        packet
            .add_question(&Question::new(
                "_res1._tcp.com".try_into().unwrap(),
                simple_dns::QTYPE::SRV,
                simple_dns::QCLASS::ANY,
                false,
            ))
            .unwrap();

        let (unicast_response, reply) = build_reply(packet, &resources).unwrap();
        let reply = reply.to_packet().unwrap();

        assert!(!unicast_response);
        assert_eq!(1, reply.answers.len());
        assert_eq!(2, reply.additional_records.len());
    }

}
