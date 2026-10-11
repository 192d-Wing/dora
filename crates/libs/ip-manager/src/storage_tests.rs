//! Backend-neutral conformance suite for [`Storage`](crate::Storage)
//! implementations.
//!
//! Every backend module instantiates [`storage_conformance_tests!`] with an
//! expression that evaluates to a future of a fresh, isolated store, e.g.
//!
//! ```ignore
//! #[cfg(test)]
//! mod conformance {
//!     storage_conformance_tests!(crate::postgres::PostgresDb::new_test());
//! }
//! ```
//!
//! The same cases then run against each backend, which is what keeps their
//! allocation and lifecycle semantics identical. `manager_tests` exercise the
//! [`IpManager`](crate::IpManager) API (they need an ICMPv4 socket);
//! `storage_level_tests` call the trait directly.

macro_rules! storage_conformance_tests {
    ($new:expr) => {
        mod manager_tests {
    use std::net::Ipv4Addr;

    use $crate::*;
    use config::LeaseTime;
    use config::v4::{NetRange, Network};
    use tracing_test::traced_test;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    // get multiple first-available IPs in a range
    // this mimics what happens when multiple clients simultaneously 'DISCOVER'
    #[tokio::test]
    #[traced_test]
    async fn test_first_available() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        let client_id = &[2, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );

        let client_id = &[3, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 102)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 102))
        );

        let client_id = &[4, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 103)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 103))
        );

        Ok(())
    }

    //
    #[tokio::test]
    #[traced_test]
    async fn test_reserve_first() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        // Expiry is stored at 1-second granularity and looked up with a strict
        // `expires_at > now`, so a 1s lease reserved just before a second boundary
        // can read back as already-expired (lookup_id -> Unreserved). Use a few
        // seconds so the immediate lookup is reliable; the sleep below still
        // outlives it to exercise the expired-entry cleanup.
        let expires_at = SystemTime::now() + Duration::from_secs(3);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        // outlive the 3s lease above so the entry is expired for the next reserve
        tokio::time::sleep(Duration::from_secs(5)).await;

        // try another range with the same client id-- should delete previous expired
        // entry
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 5, 100)..=Ipv4Addr::new(192, 168, 5, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.5.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        // long lease: this is the final lookup, so it just needs to stay valid
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;

        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100))
        );

        Ok(())
    }

    // DISCOVER - ACK
    // get lease on discover like in a rapid commit response
    #[tokio::test]
    #[traced_test]
    async fn test_first_available_ack() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        // go straight to lease
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        Ok(())
    }

    // do reserve and lease in 2 steps like usual
    #[tokio::test]
    #[traced_test]
    async fn test_lease() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(5);
        // reserve from range
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        // make lease
        mgr.try_lease([192, 168, 1, 100].into(), client_id, expires_at, &network)
            .await?;
        let ip = mgr.lookup_id(client_id).await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        Ok(())
    }

    // add some leases then select *
    #[tokio::test]
    #[traced_test]
    async fn test_select_all() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let mut res = vec![];

        let expires_at = SystemTime::now() + Duration::from_secs(30);
        for i in 0..5 {
            let client_id = &[1, 1, 1, 1, 1, 1 + i];
            // reserve from range
            let ip = mgr
                .reserve_first(&range, &network, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100 + i)));

            // make lease
            mgr.try_lease(ip, client_id, expires_at, &network).await?;
            let state = mgr.get(ip).await?.expect("not found");
            assert_eq!(
                state.as_ref().ip(),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100 + i))
            );
            assert_eq!(state.as_ref().id().unwrap(), &[1, 1, 1, 1, 1, 1 + i]);

            res.push(State::Leased(ClientInfo {
                ip: state.as_ref().ip(),
                id: Some(client_id.to_vec().clone()),
                // systtime we get back has no nano seconds
                expires_at: state.as_ref().expires_at(),
                network: network.subnet().into(),
            }));
        }

        let leases = mgr.select_all().await?;
        assert_eq!(
            leases.into_iter().collect::<HashSet<_>>(),
            res.into_iter().collect()
        );

        Ok(())
    }

    // do reserve and lease in 2 steps like usual
    #[tokio::test]
    #[traced_test]
    async fn test_lease_authoritative() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_authoritative(true);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(1);
        // reserve from range, expires in 1s
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        // different client makes a lease using just a REQUEST
        let client_id = &[1, 2, 3, 4, 5, 7];
        mgr.try_lease(
            [192, 168, 1, 101].into(),
            client_id,
            SystemTime::now() + Duration::from_secs(5),
            &network,
        )
        .await?;
        let ip = mgr.lookup_id(client_id).await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101)));

        tokio::time::sleep(Duration::from_secs(2)).await;

        // client 1's reserve expired, reserve it again
        let client_id = &[1, 2, 3, 4, 5, 8];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        // ip 100 available now since client 1 never claimed it
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        Ok(())
    }

    // reserve 2 ips then ack them both
    #[tokio::test]
    #[traced_test]
    async fn test_multiple_ranges() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range_a = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let range_b = NetRange::new(
            Ipv4Addr::new(10, 10, 1, 100)..=Ipv4Addr::new(10, 10, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network_a = Network::default();
        network_a
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range_a.clone()]);
        let mut network_b = Network::default();
        network_b
            .set_subnet("10.10.1.0/24".parse()?)
            .set_ranges(vec![range_b.clone()]);
        // reserve from range a
        {
            let client_id = &[1, 2, 3, 4, 5, 6];
            let expires_at = SystemTime::now() + Duration::from_secs(5);
            let ip = mgr
                .reserve_first(&range_a, &network_a, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        }
        // reserve from range b
        {
            let client_id = &[2, 2, 3, 4, 5, 6];
            let expires_at = SystemTime::now() + Duration::from_secs(5);
            let ip = mgr
                .reserve_first(&range_b, &network_b, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 10, 1, 100)));
        }
        mgr.try_lease(
            [192, 168, 1, 100].into(),
            &[1, 2, 3, 4, 5, 6],
            SystemTime::now() + Duration::from_secs(60),
            &network_a,
        )
        .await?;
        assert_eq!(
            mgr.lookup_id(&[1, 2, 3, 4, 5, 6]).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );
        // make other range lease
        mgr.try_lease(
            [10, 10, 1, 100].into(),
            &[2, 2, 3, 4, 5, 6],
            SystemTime::now() + Duration::from_secs(60),
            &network_b,
        )
        .await?;
        assert_eq!(
            mgr.lookup_id(&[2, 2, 3, 4, 5, 6]).await?,
            IpAddr::V4(Ipv4Addr::new(10, 10, 1, 100))
        );

        Ok(())
    }

    // programmatically fill a range
    #[tokio::test]
    #[traced_test]
    async fn test_fill_range() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // fill up range with new clients
        for range_ip in range.iter() {
            let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
            let expires_at = SystemTime::now() + Duration::from_secs(60);
            let ip = mgr
                .reserve_first(&range, &network, &client_id, expires_at, None)
                .await?;
            assert_eq!(range_ip, ip);
            assert_eq!(mgr.lookup_id(&client_id).await?, range_ip);
        }

        // range is empty, should error
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(&range, &network, &[2, 3, 4, 6, 6], expires_at, None)
            .await;
        assert!(ip.is_err());

        Ok(())
    }

    // test RELEASE
    #[tokio::test]
    #[traced_test]
    async fn test_release_ip() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(mgr.lookup_id(&client_id).await?, ip);

        // release IP
        let info = mgr.release_ip(ip, &client_id).await?;
        assert_eq!(
            info.unwrap().ip,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        // try a new client, should get the same IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );
        Ok(())
    }

    // test DECLINE
    #[tokio::test]
    #[traced_test]
    async fn test_probate_ip() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(mgr.lookup_id(&client_id).await?, ip);

        // probate IP
        mgr.probate_ip(ip, &client_id, SystemTime::now() + Duration::from_secs(180))
            .await?;
        assert!(mgr.lookup_id(&client_id).await.is_err());

        // try a new client, should skip probated IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );
        Ok(())
    }

    // test ping failure
    #[tokio::test]
    #[traced_test]
    async fn test_ping_fail() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_ping_check(true);
        // mark this IP as in-use in the DAD cache
        let ip = Ipv4Addr::new(192, 168, 1, 100);
        mgr.ping_insert(ip.into(), true).await;
        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );
        Ok(())
    }

    // test bad lookup
    #[tokio::test]
    #[traced_test]
    async fn test_bad_lookup() -> Result<()> {
        let mgr = IpManager::new($new.await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_ping_check(true);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();

        assert!(mgr.lookup_id(&client_id).await.is_err());
        Ok(())
    }

    // the generalized allocator hands out v6 addresses from a v6 config pool
    // through the same reserve_first path used by v4.
    #[tokio::test]
    #[traced_test]
    async fn test_v6_reserve_first_generalized() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let range = &net.ranges()[0];

        let mgr = IpManager::new($new.await?)?;
        let exp = SystemTime::now() + Duration::from_secs(60);

        // first two clients get sequential addresses from the pool start
        let a = mgr
            .reserve_first(range, net, &[1, 1, 1], exp, Some(IpState::Reserve))
            .await?;
        assert_eq!(a, IpAddr::V6("2001:db8:1::100".parse()?));
        let b = mgr
            .reserve_first(range, net, &[2, 2, 2], exp, Some(IpState::Reserve))
            .await?;
        assert_eq!(b, IpAddr::V6("2001:db8:1::101".parse()?));

        // the reserved v6 binding is found by DUID+IAID identity, and not by the
        // v4 identity lookup
        assert_eq!(mgr.lookup_id_v6(&[1, 1, 1]).await?, a);
        assert!(mgr.lookup_id(&[1, 1, 1]).await.is_err());

        // commit the lease (Request/Reply) and confirm it persists as Leased
        mgr.try_lease(a, &[1, 1, 1], exp, net).await?;
        assert!(matches!(mgr.get(a).await?, Some(State::Leased(_))));
        Ok(())
    }

    // renew() extends an existing binding but never creates one (Renew/Rebind)
    #[tokio::test]
    #[traced_test]
    async fn test_renew_extends_only_existing() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let range = &net.ranges()[0];

        let mgr = IpManager::new($new.await?)?;
        let now = SystemTime::now();
        let ip = mgr
            .reserve_first(
                range,
                net,
                &[7, 7, 7],
                now + Duration::from_secs(60),
                Some(IpState::Lease),
            )
            .await?;

        // existing (ip,id) -> extended
        let later = now + Duration::from_secs(120);
        assert_eq!(mgr.renew(ip, &[7, 7, 7], later).await?, Some(ip));
        // wrong id -> no binding created/extended
        assert_eq!(mgr.renew(ip, &[9, 9, 9], later).await?, None);
        // address we don't hold -> None (no insert)
        let other = IpAddr::V6("2001:db8:1::105".parse()?);
        assert_eq!(mgr.renew(other, &[7, 7, 7], later).await?, None);
        assert!(
            mgr.get(other).await?.is_none(),
            "renew must not create a binding"
        );
        Ok(())
    }

    // IA_PD: delegate /64s from a /56 pool, reuse per client, renew, release
    #[tokio::test]
    #[traced_test]
    async fn test_allocate_pd() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let pool = &net.pd_pools()[0]; // 2001:db8:100::/56 delegating /64
        let subnet = IpAddr::V6(net.full_subnet().network());

        let mgr = IpManager::new($new.await?)?;
        let exp = SystemTime::now() + Duration::from_secs(60);

        // sequential clients get consecutive /64s from the pool start
        let (p1, len) = mgr
            .allocate_pd(pool, subnet, &[1, 1, 1], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(len, 64);
        assert_eq!(p1, "2001:db8:100::".parse::<Ipv6Addr>()?);
        let (p2, _) = mgr
            .allocate_pd(pool, subnet, &[2, 2, 2], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(p2, "2001:db8:100:1::".parse::<Ipv6Addr>()?);

        // same client reuses its delegation
        let (p1b, _) = mgr
            .allocate_pd(pool, subnet, &[1, 1, 1], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(p1b, p1);

        // renew extends, release frees
        let later = exp + Duration::from_secs(60);
        assert_eq!(
            mgr.renew_pd(IpAddr::V6(p1), 64, &[1, 1, 1], later).await?,
            Some(IpAddr::V6(p1))
        );
        assert!(
            mgr.release_pd(IpAddr::V6(p1), 64, &[1, 1, 1])
                .await?
                .is_some()
        );
        // renewing a released prefix returns None (no binding)
        assert_eq!(
            mgr.renew_pd(IpAddr::V6(p1), 64, &[1, 1, 1], later).await?,
            None
        );
        Ok(())
    }

        }

        mod storage_level_tests {
    use std::collections::HashSet;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::{Duration, SystemTime};

    use $crate::{
        ConfigCandidateRecord, IpState, OperationRecord, OperationStatus,
        RuntimeReservationRecord, State, Storage,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }

    fn secs_from_now(delta: i64) -> SystemTime {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        SystemTime::UNIX_EPOCH + Duration::from_secs((now + delta) as u64)
    }

    fn op(id: &str, status: OperationStatus, created_at: SystemTime) -> OperationRecord {
        OperationRecord {
            operation_id: id.to_owned(),
            action: "test".to_owned(),
            status,
            actor: Some("tester".to_owned()),
            input_summary: None,
            result: None,
            error_code: None,
            error_message: None,
            created_at,
            started_at: None,
            completed_at: None,
        }
    }

    fn reservation(family: &str, ip: &str) -> RuntimeReservationRecord {
        RuntimeReservationRecord {
            family: family.to_owned(),
            ip: ip.to_owned(),
            prefix: None,
            network: Some("net".to_owned()),
            match_json: "{}".to_owned(),
            options_json: None,
            class: None,
            lease_time: Some(300),
            created_at: secs_from_now(0),
        }
    }

    fn candidate(id: &str, status: &str, created_at: SystemTime) -> ConfigCandidateRecord {
        ConfigCandidateRecord {
            candidate_id: id.to_owned(),
            status: status.to_owned(),
            document: "v4: {}".to_owned(),
            message: None,
            validation: None,
            created_at,
            activated_at: None,
        }
    }

    /// mimics IpManager::reserve_first's two-step: try next_expired (or id
    /// match), else insert the next available address in the range.
    async fn alloc<S: Storage>(
        db: &S,
        range: &std::ops::RangeInclusive<IpAddr>,
        excl: &HashSet<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<IpAddr, Box<dyn std::error::Error>> {
        if let Some(ip) = db
            .next_expired(
                range.clone(),
                network,
                id,
                expires_at,
                Some(IpState::Reserve),
            )
            .await?
        {
            return Ok(ip);
        }
        Ok(db
            .insert_max_in_range(
                range.clone(),
                excl,
                network,
                id,
                expires_at,
                Some(IpState::Reserve),
            )
            .await?
            .expect("range should have an available address"))
    }

    #[tokio::test]
    async fn v6_insert_get_id_release() -> TestResult {
        let db = $new.await?;
        let addr = v6("2001:db8:1::100");
        let net = v6("2001:db8:1::");
        let id: &[u8] = &[0xaa, 0xbb, 0xcc];
        let exp = SystemTime::now() + Duration::from_secs(60);

        db.insert(addr, net, id, exp, Some(IpState::Lease)).await?;

        match db.get(addr).await? {
            Some(State::Leased(info)) => {
                assert_eq!(info.ip(), addr);
                assert_eq!(info.id(), Some(id));
                assert_eq!(info.network(), net);
            }
            other => panic!("expected Leased, got {other:?}"),
        }
        assert_eq!(db.get_id_v6(id).await?, Some(addr));

        // a release carrying the wrong id must NOT delete another client's lease
        let wrong = db.release_ip(addr, &[0xde, 0xad]).await?;
        assert!(wrong.is_none(), "wrong-id release returns no info");
        assert!(
            db.get(addr).await?.is_some(),
            "lease must survive wrong-id release"
        );

        let released = db.release_ip(addr, id).await?;
        assert!(released.is_some(), "release should return prior info");
        // The row is retained but expired + unowned (reclaimable) rather than
        // deleted, so released addresses aren't leaked from the allocator.
        assert!(
            matches!(db.get(addr).await?, Some(State::Reserved(_))),
            "released address is retained as reclaimable, not leased/deleted"
        );
        assert!(
            db.get_id_v6(id).await?.is_none(),
            "released id no longer maps to a live lease"
        );
        Ok(())
    }

    #[tokio::test]
    async fn v6_next_available_sequential_honors_exclusions() -> TestResult {
        let db = $new.await?;
        let net = v6("2001:db8:1::");
        let range = v6("2001:db8:1::100")..=v6("2001:db8:1::110");
        let exp = SystemTime::now() + Duration::from_secs(60);
        let mut excl = HashSet::new();
        excl.insert(v6("2001:db8:1::101"));

        // empty range -> start of range
        assert_eq!(
            alloc(&db, &range, &excl, net, &[1], exp).await?,
            v6("2001:db8:1::100")
        );
        // ::101 excluded -> ::102
        assert_eq!(
            alloc(&db, &range, &excl, net, &[2], exp).await?,
            v6("2001:db8:1::102")
        );
        // then ::103
        assert_eq!(
            alloc(&db, &range, &excl, net, &[3], exp).await?,
            v6("2001:db8:1::103")
        );

        // same id returns its existing address (idempotent via next_expired id-match)
        assert_eq!(
            alloc(&db, &range, &excl, net, &[2], exp).await?,
            v6("2001:db8:1::102")
        );
        Ok(())
    }

    #[tokio::test]
    async fn v6_empty_range_skips_excluded_start() -> TestResult {
        // regression: an empty range whose first address is excluded must not
        // hand out that excluded address (which would then be rejected and loop).
        let db = $new.await?;
        let net = v6("2001:db8:1::");
        let range = v6("2001:db8:1::100")..=v6("2001:db8:1::110");
        let exp = SystemTime::now() + Duration::from_secs(60);
        let mut excl = HashSet::new();
        excl.insert(v6("2001:db8:1::100")); // exclude the start

        assert_eq!(
            alloc(&db, &range, &excl, net, &[1], exp).await?,
            v6("2001:db8:1::101")
        );
        Ok(())
    }

    #[tokio::test]
    async fn v6_reserve_then_lease_via_update_unexpired() -> TestResult {
        let db = $new.await?;
        let addr = v6("2001:db8:1::100");
        let net = v6("2001:db8:1::");
        let id: &[u8] = &[9, 9, 9];
        let exp = SystemTime::now() + Duration::from_secs(60);

        // reserve (offer), then transition to lease (request/reply)
        db.insert(addr, net, id, exp, Some(IpState::Reserve))
            .await?;
        let leased = db
            .update_unexpired(addr, IpState::Lease, id, exp, Some(id))
            .await?;
        assert_eq!(leased, Some(addr));
        assert!(matches!(db.get(addr).await?, Some(State::Leased(_))));
        Ok(())
    }

    #[tokio::test]
    async fn v4_and_v6_coexist_in_separate_tables() -> TestResult {
        let db = $new.await?;
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let v4net = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0));
        let a6 = v6("2001:db8:1::100");
        let n6 = v6("2001:db8:1::");
        let exp = SystemTime::now() + Duration::from_secs(60);

        db.insert(v4, v4net, &[1, 2, 3], exp, Some(IpState::Lease))
            .await?;
        db.insert(a6, n6, &[4, 5, 6], exp, Some(IpState::Lease))
            .await?;

        assert!(matches!(db.get(v4).await?, Some(State::Leased(i)) if i.ip() == v4));
        assert!(matches!(db.get(a6).await?, Some(State::Leased(i)) if i.ip() == a6));
        assert_eq!(db.get_id_v6(&[4, 5, 6]).await?, Some(a6));
        assert_eq!(db.get_id(&[1, 2, 3]).await?, Some(v4));
        // family lookups do not cross tables: v6 id absent from v4 table and vice versa
        assert_eq!(db.get_id(&[4, 5, 6]).await?, None);
        assert_eq!(db.get_id_v6(&[1, 2, 3]).await?, None);

        // select_all and count(Lease) include both families
        assert_eq!(db.select_all().await?.len(), 2);
        assert_eq!(db.count(IpState::Lease).await?, 2);
        Ok(())
    }


    // ----- semantics every backend must share --------------------------------

    #[tokio::test]
    async fn insert_pk_conflict_errors() -> TestResult {
        let db = $new.await?;
        let (ip, net) = (v4("10.1.0.5"), v4("10.1.0.0"));
        let exp = secs_from_now(60);
        db.insert(ip, net, &[1], exp, Some(IpState::Lease)).await?;
        assert!(
            db.insert(ip, net, &[2], exp, Some(IpState::Lease)).await.is_err(),
            "a second insert of the same address must fail (try_lease maps it to AddrInUse)"
        );
        // the original owner is untouched
        assert_eq!(db.get(ip).await?.unwrap().as_ref().id(), Some(&[1u8][..]));
        Ok(())
    }

    #[tokio::test]
    async fn expiry_comparison_is_strict() -> TestResult {
        let db = $new.await?;
        let net = v4("10.2.0.0");
        // expires_at == now (second-truncated) is neither live nor expired
        let edge = v4("10.2.0.1");
        db.insert(edge, net, &[1], secs_from_now(0), Some(IpState::Lease)).await?;
        assert_eq!(db.get_id(&[1]).await?, None, "expires_at == now is not live");
        assert_eq!(db.count(IpState::Lease).await?, 0);
        // clearly expired: anyone may take it over
        let old = v4("10.2.0.2");
        db.insert(old, net, &[2], secs_from_now(-10), Some(IpState::Lease)).await?;
        assert!(db.update_expired(old, Some(IpState::Lease), &[9], secs_from_now(60)).await?);
        assert_eq!(db.get(old).await?.unwrap().as_ref().id(), Some(&[9u8][..]));
        // live: only the owner may update it
        let live = v4("10.2.0.3");
        db.insert(live, net, &[3], secs_from_now(60), Some(IpState::Lease)).await?;
        assert!(!db.update_expired(live, Some(IpState::Lease), &[9], secs_from_now(90)).await?);
        assert!(db.update_expired(live, Some(IpState::Lease), &[3], secs_from_now(90)).await?);
        assert_eq!(db.get_id(&[3]).await?, Some(live));
        Ok(())
    }

    #[tokio::test]
    async fn update_unexpired_none_id_clears_client() -> TestResult {
        let db = $new.await?;
        let (ip, net) = (v4("10.3.0.5"), v4("10.3.0.0"));
        db.insert(ip, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?;
        // probate_ip: declined address loses its owner
        assert_eq!(
            db.update_unexpired(ip, IpState::Probate, &[1], secs_from_now(120), None).await?,
            Some(ip)
        );
        match db.get(ip).await? {
            Some(State::Probated(info)) => assert_eq!(info.id(), None),
            other => panic!("expected Probated with no owner, got {other:?}"),
        }
        assert_eq!(db.get_id(&[1]).await?, None);
        // wrong id / expired rows are not touched
        let other = v4("10.3.0.6");
        db.insert(other, net, &[2], secs_from_now(-5), Some(IpState::Lease)).await?;
        assert_eq!(
            db.update_unexpired(other, IpState::Lease, &[2], secs_from_now(60), Some(&[2])).await?,
            None,
            "expired rows are not renewable via update_unexpired"
        );
        Ok(())
    }

    #[tokio::test]
    async fn release_ip_keeps_row_expired_unowned() -> TestResult {
        let db = $new.await?;
        let (ip, net) = (v4("10.4.0.5"), v4("10.4.0.0"));
        db.insert(ip, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?;
        let prior = db.release_ip(ip, &[1]).await?.expect("release returns prior info");
        assert_eq!(prior.id(), Some(&[1u8][..]));
        assert_eq!(prior.ip(), ip);
        match db.get(ip).await? {
            Some(State::Reserved(info)) => {
                assert_eq!(info.id(), None);
                assert_eq!(info.expires_at(), SystemTime::UNIX_EPOCH);
                assert_eq!(info.network(), net, "network is retained");
            }
            other => panic!("expected retained Reserved row, got {other:?}"),
        }
        assert_eq!(db.get_id(&[1]).await?, None);
        // and it is immediately reclaimable
        let range = v4("10.4.0.1")..=v4("10.4.0.10");
        assert_eq!(
            db.next_expired(range, net, &[7], secs_from_now(60), Some(IpState::Lease)).await?,
            Some(ip)
        );
        Ok(())
    }

    #[tokio::test]
    async fn next_expired_picks_lowest_expired_before_own_row_and_clears_probation() -> TestResult {
        let db = $new.await?;
        let net = v4("10.5.0.0");
        let (a, b, c) = (v4("10.5.0.100"), v4("10.5.0.101"), v4("10.5.0.102"));
        db.insert(a, net, &[0xaa], secs_from_now(-10), Some(IpState::Probate)).await?; // expired, other
        db.insert(b, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?; // live, ours
        db.insert(c, net, &[0xcc], secs_from_now(-10), Some(IpState::Lease)).await?; // expired
        let range = v4("10.5.0.100")..=v4("10.5.0.110");
        let got = db
            .next_expired(range, net, &[1], secs_from_now(60), Some(IpState::Lease))
            .await?;
        assert_eq!(got, Some(a), "the lowest expired address wins over our own live row");
        match db.get(a).await? {
            Some(State::Leased(info)) => assert_eq!(info.id(), Some(&[1u8][..])),
            other => panic!("expected Leased by us (probation cleared), got {other:?}"),
        }
        // our live row at b is left alone
        assert_eq!(db.get(b).await?.unwrap().as_ref().id(), Some(&[1u8][..]));
        Ok(())
    }

    #[tokio::test]
    async fn next_expired_id_match_is_range_bounded() -> TestResult {
        let db = $new.await?;
        let net = v4("10.6.0.0");
        db.insert(v4("10.6.0.5"), net, &[1], secs_from_now(60), Some(IpState::Lease)).await?;
        let other_range = v4("192.168.1.100")..=v4("192.168.1.110");
        assert_eq!(
            db.next_expired(other_range, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?,
            None,
            "a binding outside the range must not be reused"
        );
        Ok(())
    }

    #[tokio::test]
    async fn insert_max_in_range_excluded_max_does_not_skip() -> TestResult {
        let db = $new.await?;
        let net = v4("10.7.0.0");
        let range = v4("10.7.0.100")..=v4("10.7.0.110");
        let mut excl = HashSet::new();
        excl.insert(v4("10.7.0.105"));
        excl.insert(v4("10.7.0.107"));
        for i in 100..=105u8 {
            db.insert(v4(&format!("10.7.0.{i}")), net, &[i], secs_from_now(60), Some(IpState::Lease))
                .await?;
        }
        // max (.105) is itself excluded (an exclusion added after it was
        // allocated): the next free address is .106, not .107 (#62)
        let got = db
            .insert_max_in_range(range.clone(), &excl, net, &[0xee], secs_from_now(60), Some(IpState::Reserve))
            .await?;
        assert_eq!(got, Some(v4("10.7.0.106")));
        // and an excluded address above max is still stepped over
        let got = db
            .insert_max_in_range(range, &excl, net, &[0xef], secs_from_now(60), Some(IpState::Reserve))
            .await?;
        assert_eq!(got, Some(v4("10.7.0.108")));
        Ok(())
    }

    #[tokio::test]
    async fn insert_max_in_range_exhausted_returns_none() -> TestResult {
        let db = $new.await?;
        let net = v4("10.8.0.0");
        let range = v4("10.8.0.1")..=v4("10.8.0.3");
        let excl = HashSet::new();
        for i in 1..=3u8 {
            assert_eq!(
                db.insert_max_in_range(range.clone(), &excl, net, &[i], secs_from_now(60), None)
                    .await?,
                Some(v4(&format!("10.8.0.{i}")))
            );
        }
        assert_eq!(
            db.insert_max_in_range(range.clone(), &excl, net, &[9], secs_from_now(60), None)
                .await?,
            None
        );
        // a fully excluded empty range is also exhausted
        let range2 = v4("10.8.1.1")..=v4("10.8.1.2");
        let all: HashSet<IpAddr> = [v4("10.8.1.1"), v4("10.8.1.2")].into_iter().collect();
        assert_eq!(
            db.insert_max_in_range(range2, &all, net, &[9], secs_from_now(60), None).await?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn pd_ia_na_separation() -> TestResult {
        let db = $new.await?;
        let addr = v6("2001:db8:2::1");
        let net = v6("2001:db8:2::");
        let exp = secs_from_now(60);
        db.insert(addr, net, &[1], exp, Some(IpState::Lease)).await?;
        db.upsert_pd(addr, 64, net, &[2], exp, Some(IpState::Lease)).await?;
        // same address, two bindings
        assert_eq!(db.get(addr).await?.unwrap().as_ref().id(), Some(&[1u8][..]));
        assert_eq!(db.get_pd(addr, 64).await?.unwrap().as_ref().id(), Some(&[2u8][..]));
        // a /128 "prefix" addresses the IA_NA binding
        assert_eq!(db.get_pd(addr, 128).await?.unwrap().as_ref().id(), Some(&[1u8][..]));
        assert_eq!(db.get_id_v6(&[1]).await?, Some(addr));
        assert_eq!(db.get_id_v6(&[2]).await?, None, "PD rows are not IA_NA");
        assert_eq!(db.get_id_pd(&[2]).await?, Some((addr, 64)));
        assert_eq!(db.get_id_pd(&[1]).await?, None, "IA_NA rows are not PD");
        assert_eq!(db.select_all().await?.len(), 2);
        assert_eq!(db.count(IpState::Lease).await?, 2, "count includes PD");
        // delete only removes the IA_NA row
        db.delete(addr).await?;
        assert!(db.get(addr).await?.is_none());
        assert!(db.get_pd(addr, 64).await?.is_some());
        // v4 inputs are a no-op
        assert!(db.get_pd(v4("10.0.0.1"), 24).await?.is_none());
        assert!(!db.claim_pd(v4("10.0.0.1"), 24, v4("10.0.0.0"), &[1], exp, None).await?);
        Ok(())
    }

    #[tokio::test]
    async fn claim_pd_free_expired_own_vs_blocked() -> TestResult {
        let db = $new.await?;
        let p = v6("2001:db8:3:100::");
        let net = v6("2001:db8:3::");
        // free
        assert!(db.claim_pd(p, 56, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?);
        // live and someone else's: blocked, owner unchanged
        assert!(!db.claim_pd(p, 56, net, &[2], secs_from_now(60), Some(IpState::Lease)).await?);
        assert_eq!(db.get_pd(p, 56).await?.unwrap().as_ref().id(), Some(&[1u8][..]));
        // live and ours: ok
        assert!(db.claim_pd(p, 56, net, &[1], secs_from_now(120), Some(IpState::Lease)).await?);
        // expired: anyone may take it
        db.upsert_pd(p, 56, net, &[1], secs_from_now(-10), Some(IpState::Lease)).await?;
        assert!(db.claim_pd(p, 56, net, &[2], secs_from_now(60), Some(IpState::Lease)).await?);
        assert_eq!(db.get_pd(p, 56).await?.unwrap().as_ref().id(), Some(&[2u8][..]));
        // renew: only owner + live; sets leased
        assert_eq!(db.renew_pd(p, 56, &[1], secs_from_now(90)).await?, None);
        assert_eq!(db.renew_pd(p, 56, &[2], secs_from_now(90)).await?, Some(p));
        assert!(matches!(db.get_pd(p, 56).await?, Some(State::Leased(_))));
        Ok(())
    }

    #[tokio::test]
    async fn release_pd_deletes_and_returns_prior() -> TestResult {
        let db = $new.await?;
        let p = v6("2001:db8:4:200::");
        let net = v6("2001:db8:4::");
        db.upsert_pd(p, 60, net, &[1], secs_from_now(60), Some(IpState::Lease)).await?;
        assert!(db.release_pd(p, 60, &[9]).await?.is_none(), "wrong id releases nothing");
        assert!(db.get_pd(p, 60).await?.is_some());
        let prior = db.release_pd(p, 60, &[1]).await?.expect("prior info");
        assert_eq!(prior.ip(), p);
        assert_eq!(prior.id(), Some(&[1u8][..]));
        assert!(db.get_pd(p, 60).await?.is_none(), "released delegations are deleted");
        assert_eq!(db.get_id_pd(&[1]).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn operations_dup_errors_update_absent_noop_created_at_kept() -> TestResult {
        let db = $new.await?;
        let created = secs_from_now(-100);
        db.insert_operation(&op("op-1", OperationStatus::Accepted, created)).await?;
        assert!(
            db.insert_operation(&op("op-1", OperationStatus::Running, created)).await.is_err(),
            "duplicate operation id must error"
        );
        // update of an unknown id is a silent no-op
        db.update_operation(&op("nope", OperationStatus::Failed, created)).await?;
        assert!(db.get_operation("nope").await?.is_none());
        // update never rewrites created_at
        let mut upd = op("op-1", OperationStatus::Succeeded, secs_from_now(0));
        upd.completed_at = Some(secs_from_now(0));
        db.update_operation(&upd).await?;
        let got = db.get_operation("op-1").await?.expect("exists");
        assert_eq!(got.status, OperationStatus::Succeeded);
        assert_eq!(got.created_at, created);
        assert_eq!(got.completed_at, upd.completed_at);
        Ok(())
    }

    #[tokio::test]
    async fn reservations_upsert_get_delete_list_ordered() -> TestResult {
        let db = $new.await?;
        db.upsert_reservation(&reservation("v6", "2001:db8::10")).await?;
        db.upsert_reservation(&reservation("v4", "10.0.0.20")).await?;
        db.upsert_reservation(&reservation("v4", "10.0.0.10")).await?;
        let mut again = reservation("v4", "10.0.0.10");
        again.class = Some("printers".to_owned());
        db.upsert_reservation(&again).await?; // upsert replaces
        let got = db.get_reservation("v4", "10.0.0.10").await?.expect("exists");
        assert_eq!(got.class.as_deref(), Some("printers"));
        let keys: Vec<(String, String)> = db
            .list_reservations()
            .await?
            .into_iter()
            .map(|r| (r.family, r.ip))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("v4".to_owned(), "10.0.0.10".to_owned()),
                ("v4".to_owned(), "10.0.0.20".to_owned()),
                ("v6".to_owned(), "2001:db8::10".to_owned()),
            ],
            "ordered by (family, ip)"
        );
        assert!(db.delete_reservation("v4", "10.0.0.20").await?);
        assert!(!db.delete_reservation("v4", "10.0.0.20").await?);
        assert_eq!(db.list_reservations().await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn candidates_list_desc_active_latest_and_activate() -> TestResult {
        let db = $new.await?;
        db.upsert_config_candidate(&candidate("c-old", "valid", secs_from_now(-300))).await?;
        db.upsert_config_candidate(&candidate("c-new", "valid", secs_from_now(-100))).await?;
        let ids: Vec<String> = db
            .list_config_candidates()
            .await?
            .into_iter()
            .map(|c| c.candidate_id)
            .collect();
        assert_eq!(ids, vec!["c-new".to_owned(), "c-old".to_owned()], "created_at DESC");
        assert!(db.active_config_candidate().await?.is_none());

        // capture the timestamps once: `secs_from_now` re-reads the clock and a
        // second boundary between two calls would shift the expected value
        let (act_old, act_new) = (secs_from_now(-50), secs_from_now(-10));
        db.activate_config_candidate("c-old", act_old).await?;
        db.activate_config_candidate("c-new", act_new).await?;
        let active = db.active_config_candidate().await?.expect("active");
        assert_eq!(active.candidate_id, "c-new");
        assert_eq!(active.status, "activated");
        assert_eq!(active.activated_at, Some(act_new));
        let old = db.get_config_candidate("c-old").await?.expect("exists");
        assert_eq!(old.status, "superseded", "activation supersedes in the same txn");
        Ok(())
    }

    #[tokio::test]
    async fn activate_unknown_id_leaves_none_active() -> TestResult {
        let db = $new.await?;
        db.upsert_config_candidate(&candidate("c-a", "valid", secs_from_now(-100))).await?;
        db.activate_config_candidate("c-a", secs_from_now(-50)).await?;
        assert_eq!(db.active_config_candidate().await?.unwrap().candidate_id, "c-a");
        db.activate_config_candidate("does-not-exist", secs_from_now(0)).await?;
        assert_eq!(db.get_config_candidate("c-a").await?.unwrap().status, "superseded");
        assert!(db.active_config_candidate().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn server_mode_fresh_is_normal_then_set() -> TestResult {
        let db = $new.await?;
        assert_eq!(db.get_server_mode().await?, Some("normal".to_owned()));
        db.set_server_mode("drain").await?;
        assert_eq!(db.get_server_mode().await?, Some("drain".to_owned()));
        db.set_server_mode("normal").await?;
        assert_eq!(db.get_server_mode().await?, Some("normal".to_owned()));
        Ok(())
    }

    /// Concurrent DISCOVERs must never be handed the same address. Mirrors the
    /// two-step allocation of `IpManager::reserve_first`, retrying on a storage
    /// error (Postgres may report a key conflict under a race).
    #[tokio::test]
    async fn concurrent_alloc_unique() -> TestResult {
        let db = std::sync::Arc::new($new.await?);
        let range = v4("10.9.0.1")..=v4("10.9.0.200");
        let net = v4("10.9.0.0");
        let exp = secs_from_now(60);
        let mut handles = Vec::new();
        for i in 0u16..16 {
            let (db, range) = (std::sync::Arc::clone(&db), range.clone());
            handles.push(tokio::spawn(async move {
                let id = i.to_be_bytes();
                // Postgres resolves a race with a key conflict for every loser,
                // so one round of contention settles one task; under coverage
                // instrumentation it can take many rounds. Back off a little.
                let excl = HashSet::new();
                for attempt in 0..200u64 {
                    // the Box<dyn Error> is dropped before the await below (it is
                    // not Send)
                    if let Ok(ip) = alloc(&*db, &range, &excl, net, &id, exp).await {
                        return Some(ip);
                    }
                    tokio::time::sleep(Duration::from_millis(attempt.min(20))).await;
                }
                None
            }));
        }
        let mut seen = HashSet::new();
        for h in handles {
            let ip = h.await.unwrap().expect("allocation succeeded within retries");
            assert!(range.contains(&ip));
            assert!(seen.insert(ip), "duplicate allocation {ip}");
        }
        assert_eq!(seen.len(), 16);
        assert_eq!(db.count(IpState::Reserve).await?, 16);
        Ok(())
    }
        }
    };
}
