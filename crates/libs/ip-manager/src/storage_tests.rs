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

    use $crate::{IpState, State, Storage};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
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

        }
    };
}
