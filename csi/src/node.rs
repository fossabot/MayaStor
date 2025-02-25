use crate::csi::*;
use futures::future::{err, ok, Either, Future, FutureResult};
use jsonrpc;
use rpc::jsonrpc as jsondata;
use std::{boxed::Box, fs, io::ErrorKind, path::PathBuf, vec::Vec};
use tower_grpc::{Code, Request, Response, Status};

use crate::{
    mount::{match_mount, mount_fs, mount_opts_compare, unmount_fs, Fs},
    nbd::{self, nbd_stage_volume},
};

#[derive(Clone, Debug)]
pub struct Node {
    pub node_name: String,
    pub socket: String,
    pub addr: String,
    pub port: u16,
    pub filesystems: Vec<Fs>,
}

// Shortcut for creating grpc error, logging it and exiting from function
#[macro_export]
macro_rules! grpc_return {
    ($code:expr, $message:expr) => {{
        error!("{}", $message.to_string());
        return Box::new(err(Status::new($code, $message.to_string())));
    }};
}

// Determine if given access mode in conjunction with ro mount flag makes
// sense or not. If access mode is not supported or the combination does
// not make sense, return error string.
//
// NOTE: Following is based on our limited understanding of access mode
// meaning. Access mode does not control if the mount is rw/ro (that is
// rather part of the mount flags). Access mode serves as advisory info
// for CO when attaching volumes to pods. It is out of scope of storage
// plugin running on particular node to check that access mode for particular
// publish or stage request makes sense.
fn check_access_mode(
    volume_id: &str,
    access_mode: &Option<volume_capability::AccessMode>,
    readonly: bool,
) -> Result<(), String> {
    use crate::csi::volume_capability::access_mode::Mode;

    let rdonly_access_mode = match access_mode {
        Some(m) => match Mode::from_i32(m.mode).unwrap() {
            Mode::SingleNodeWriter => false,
            Mode::SingleNodeReaderOnly => true,
            Mode::MultiNodeReaderOnly => true,
            Mode::MultiNodeSingleWriter => false,
            _ => {
                return Err(format!(
                    "Unsupported publish volume mode {:?} for {}",
                    Mode::from_i32(m.mode),
                    volume_id
                ));
            }
        },
        None => return Err(format!("Missing publish mode for {}", volume_id)),
    };
    if !readonly && rdonly_access_mode {
        Err(format!(
            "Invalid combination of access mode and ro mount flag for {}",
            volume_id,
        ))
    } else {
        Ok(())
    }
}

impl Node {}

impl server::Node for Node {
    type NodeGetInfoFuture =
        FutureResult<Response<NodeGetInfoResponse>, Status>;
    type NodeGetCapabilitiesFuture =
        FutureResult<Response<NodeGetCapabilitiesResponse>, Status>;
    type NodePublishVolumeFuture = Box<
        dyn Future<Item = Response<NodePublishVolumeResponse>, Error = Status>
            + Send,
    >;
    type NodeUnpublishVolumeFuture = Box<
        dyn Future<
                Item = Response<NodeUnpublishVolumeResponse>,
                Error = Status,
            > + Send,
    >;
    type NodeGetVolumeStatsFuture = Box<
        dyn Future<Item = Response<NodeGetVolumeStatsResponse>, Error = Status>
            + Send,
    >;
    type NodeStageVolumeFuture = Box<
        dyn Future<Item = Response<NodeStageVolumeResponse>, Error = Status>
            + Send,
    >;
    type NodeUnstageVolumeFuture = Box<
        dyn Future<Item = Response<NodeUnstageVolumeResponse>, Error = Status>
            + Send,
    >;
    type NodeExpandVolumeFuture =
        FutureResult<Response<NodeExpandVolumeResponse>, Status>;

    fn node_get_info(
        &mut self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Self::NodeGetInfoFuture {
        let node_id = format!(
            "mayastor://{}/{}:{}",
            &self.node_name, &self.addr, self.port,
        );
        let max_volumes_per_node = nbd::NbdDevInfo::num_devices() as i64;

        debug!(
            "NodeGetInfo request: ID={}, max volumes={}",
            node_id, max_volumes_per_node
        );

        ok(Response::new(NodeGetInfoResponse {
            node_id,
            max_volumes_per_node,
            accessible_topology: None,
        }))
    }

    fn node_get_capabilities(
        &mut self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Self::NodeGetCapabilitiesFuture {
        let caps = vec![
            node_service_capability::rpc::Type::GetVolumeStats,
            node_service_capability::rpc::Type::StageUnstageVolume,
        ];

        debug!("NodeGetCapabilities request: {:?}", caps);

        // We don't support stage/unstage and expand volume rpcs
        ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: caps
                .into_iter()
                .map(|c| NodeServiceCapability {
                    r#type: Some(node_service_capability::Type::Rpc(
                        node_service_capability::Rpc {
                            r#type: c as i32,
                        },
                    )),
                })
                .collect(),
        }))
    }

    /// This RPC is called by the CO when a workload that wants to use the
    /// specified volume is placed (scheduled) on a node. The Plugin SHALL
    /// assume that this RPC will be executed on the node where the volume will
    /// be used. If the corresponding Controller Plugin has
    /// PUBLISH_UNPUBLISH_VOLUME controller capability, the CO MUST guarantee
    /// that this RPC is called after ControllerPublishVolume is called for the
    /// given volume on the given node and returns a success. This operation
    /// MUST be idempotent. If the volume corresponding to the volume_id has
    /// already been published at the specified target_path, and is compatible
    /// with the specified volume_capability and readonly flag, the Plugin MUST
    /// reply 0 OK. If this RPC failed, or the CO does not know if it failed or
    /// not, it MAY choose to call NodePublishVolume again, or choose to call
    /// NodeUnpublishVolume. This RPC MAY be called by the CO multiple times on
    /// the same node for the same volume with possibly different target_path
    /// and/or other arguments if the volume has MULTI_NODE capability (i.e.,
    /// access_mode is either MULTI_NODE_READER_ONLY, MULTI_NODE_SINGLE_WRITER
    /// or MULTI_NODE_MULTI_WRITER).
    fn node_publish_volume(
        &mut self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Self::NodePublishVolumeFuture {
        let msg = request.into_inner();

        trace!("{:?}", msg);

        let staging_path = &msg.staging_target_path;
        let target_path = &msg.target_path;
        let volume_id = &msg.volume_id;

        // According to the spec, the staging path is optional, but must be set
        // if the plugin advertises stage volume -- which we do so here we go.
        if staging_path == "" || target_path == "" {
            grpc_return!(
                Code::InvalidArgument,
                format!("Invalid target or staging path for {}", volume_id)
            );
        }

        // TODO: Support raw volumes
        let mnt = match msg.volume_capability.as_ref().unwrap().access_type {
            Some(volume_capability::AccessType::Mount(ref m)) => m,
            Some(volume_capability::AccessType::Block(_)) => {
                grpc_return!(
                    Code::InvalidArgument,
                    "Raw block not ratified yet"
                );
            }
            None => {
                grpc_return!(
                    Code::InvalidArgument,
                    format!("Missing access type for {}", volume_id)
                );
            }
        };

        // apparently, it does not matter what the source (device) is
        // to me thats odd but thats how the spec says it today
        if match_mount(None, Some(staging_path), true).is_none() {
            grpc_return!(
                Code::InvalidArgument,
                format!(
                    "No mount {} for volume {} (hint: volume unstaged?)",
                    staging_path, volume_id
                )
            );
        }

        if let Err(reason) = check_access_mode(
            volume_id,
            &msg.volume_capability.as_ref().unwrap().access_mode,
            msg.readonly,
        ) {
            grpc_return!(Code::InvalidArgument, reason);
        };

        let filesystem = if mnt.fs_type.is_empty() {
            &self.filesystems[0]
        } else {
            match self.filesystems.iter().find(|ent| ent.name == mnt.fs_type) {
                Some(fs) => fs,
                None => {
                    grpc_return!(
                        Code::InvalidArgument,
                        format!("Filesystem {} is not supported", mnt.fs_type)
                    );
                }
            }
        };
        let mut mnt_flags = mnt.mount_flags.clone();

        if msg.readonly {
            mnt_flags.push("ro".into());
        } else {
            mnt_flags.push("rw".into());
        }

        mnt_flags.extend(filesystem.defaults.clone());

        if let Some(mount) =
            match_mount(Some(staging_path), Some(target_path), true)
        {
            // we are already mounted check flags, if they match return OK
            let equal =
                mount_opts_compare(&mnt_flags, &mount.opts, msg.readonly);

            if equal {
                info!("Already mounted with compatible flags");
                return Box::new(ok(Response::new(
                    NodePublishVolumeResponse {},
                )));
            } else {
                // this is just to provide more context around the error
                grpc_return!(
                        Code::AlreadyExists,
                        "Failed to publish volume, already exists with incompatible flags".to_string()
                    );
            }
        }

        // if we are here, it means that we mount it for the first time or -- we
        // are mounting the same staged volume again to a different target.
        if let Err(err) = fs::create_dir_all(PathBuf::from(target_path)) {
            grpc_return!(
                Code::Internal,
                format!(
                    "Failed to create mountpoint {} for volume {}: {}",
                    target_path, volume_id, err
                )
            );
        }
        if let Err(err) = mount_fs(
            &staging_path,
            &target_path,
            true,
            &filesystem.name,
            &mnt_flags,
        ) {
            grpc_return!(
                Code::Internal,
                format!("Failed to publish volume {}: {}", volume_id, err)
            )
        } else {
            info!("Published volume {}", volume_id);
            Box::new(ok(Response::new(NodePublishVolumeResponse {})))
        }
    }

    // This RPC is called by the CO when a workload that wants to use the
    // specified volume is placed (scheduled) on a node. The Plugin SHALL assume
    // that this RPC will be executed on the node where the volume will be used.
    //
    // If the corresponding Controller Plugin has PUBLISH_UNPUBLISH_VOLUME
    // controller capability, the CO MUST guarantee that this RPC is called
    // after ControllerPublishVolume is called for the given volume on the given
    // node and returns a success.
    //
    // This operation MUST be idempotent.
    fn node_unpublish_volume(
        &mut self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Self::NodeUnpublishVolumeFuture {
        let msg = request.into_inner();

        trace!("{:?}", msg);

        let target_path = &msg.target_path;
        let volume_id = &msg.volume_id;

        // TODO: Support raw volumes
        match match_mount(None, Some(target_path), true) {
            Some(_) => {
                debug!("Unmount volume {} at {}...", volume_id, target_path);

                if let Err(err) = unmount_fs(target_path, true) {
                    grpc_return!(
                        Code::Internal,
                        format!(
                            "Failed to unpublish volume {}: {}",
                            volume_id, err
                        )
                    );
                }
                info!("Unpublished volume {} at {}", volume_id, target_path);
            }
            None => error!("Volume {} is not published", volume_id),
        }

        Box::new(ok(Response::new(NodeUnpublishVolumeResponse {})))
    }

    fn node_get_volume_stats(
        &mut self,
        request: Request<NodeGetVolumeStatsRequest>,
    ) -> Self::NodeGetVolumeStatsFuture {
        let msg = request.into_inner();
        trace!("{:?}", msg);
        // self is a reference and we can't use it in the closure below
        let socket = self.socket.clone();
        let volume_id = msg.volume_id;

        let bdev_to_stats = |bdev: jsondata::Bdev| {
            NodeGetVolumeStatsResponse {
                usage: vec![VolumeUsage {
                    total: i64::from(bdev.block_size) * bdev.num_blocks as i64,
                    unit: volume_usage::Unit::Bytes as i32,
                    // TODO: set available and used when we know how to
                    // find out their values
                    available: 0,
                    used: 0,
                }],
            }
        };

        let f = nbd::get_nbd_instance(&self.socket, &volume_id)
            .and_then(move |res| {
                if let Some(disk) = res {
                    assert_eq!(disk.bdev_name, volume_id);
                    Either::A(
                        jsonrpc::call(
                            &socket,
                            "get_bdevs",
                            Some(jsondata::GetBdevsArgs {
                                name: volume_id.to_owned(),
                            }),
                        )
                        .map_err(|err| err.into_status())
                        .and_then(move |mut bdevs: Vec<jsondata::Bdev>| {
                            if bdevs.is_empty() {
                                return err(Status::new(
                                    Code::Internal,
                                    format!("Cannot find underlying bdev for volume {}", volume_id),
                                ));
                            }
                            assert_eq!(bdevs.len(), 1);
                            ok(Response::new(bdev_to_stats(bdevs.remove(0))))
                        }),
                    )
                } else {
                    Either::B(err(Status::new(
                        Code::NotFound,
                        format!("Volume {} not found", volume_id),
                    )))
                }
            });
        Box::new(f)
    }

    fn node_expand_volume(
        &mut self,
        request: Request<NodeExpandVolumeRequest>,
    ) -> Self::NodeExpandVolumeFuture {
        let msg = request.into_inner();
        error!("Unimplemented {:?}", msg);
        err(Status::new(Code::Unimplemented, "Method not implemented"))
    }

    /// stage a volume means that we grab the raw block device and format it if
    /// so needed depending on the egress type (nbd or nvmf) call the proper
    /// implementation
    fn node_stage_volume(
        &mut self,
        request: Request<NodeStageVolumeRequest>,
    ) -> Self::NodeStageVolumeFuture {
        let msg = request.into_inner();
        let volume_id = msg.volume_id.clone();

        trace!("{:?}", msg);

        if msg.staging_target_path == "" || msg.volume_id == "" {
            grpc_return!(
                Code::InvalidArgument,
                "Invalid target path or volume id"
            );
        }

        if msg.volume_capability.is_none() {
            grpc_return!(
                Code::InvalidArgument,
                format!("No volume capabilities provided for {}", volume_id)
            );
        }

        // TODO: support raw block volumes
        let mnt = match msg.volume_capability.as_ref().unwrap().access_type {
            Some(volume_capability::AccessType::Mount(ref m)) => m.clone(),
            Some(volume_capability::AccessType::Block(_)) => grpc_return!(
                Code::InvalidArgument,
                "Raw block support is not supported"
            ),
            None => grpc_return!(
                Code::InvalidArgument,
                format!("Missing access type for volume {}", volume_id)
            ),
        };

        if let Err(reason) = check_access_mode(
            &volume_id,
            &msg.volume_capability.as_ref().unwrap().access_mode,
            // relax the check a bit by pretending all stage mounts are ro
            true,
        ) {
            grpc_return!(Code::InvalidArgument, reason);
        };

        let filesystem = if mnt.fs_type.is_empty() {
            self.filesystems[0].clone()
        } else {
            match self.filesystems.iter().find(|ent| ent.name == mnt.fs_type) {
                Some(fs) => fs.clone(),
                None => {
                    grpc_return!(
                        Code::InvalidArgument,
                        format!("Filesystem {} is not supported", mnt.fs_type)
                    );
                }
            }
        };

        debug!(
            "Staging volume {} to {}",
            volume_id, msg.staging_target_path
        );

        if let Err(err) =
            fs::create_dir_all(PathBuf::from(&msg.staging_target_path))
        {
            if err.kind() != ErrorKind::AlreadyExists {
                grpc_return!(
                    Code::Internal,
                    format!(
                        "Failed to create mountpoint {} for volume {}: {}",
                        &msg.staging_target_path, volume_id, err
                    )
                );
            }
        }

        nbd_stage_volume(self.socket.clone(), &msg, filesystem, mnt.mount_flags)
    }
    // A Node Plugin MUST implement this RPC call if it has STAGE_UNSTAGE_VOLUME
    // node capability. This RPC is a reverse operation of NodeStageVolume.
    // This RPC MUST undo the work by the corresponding NodeStageVolume. This
    // RPC SHALL be called by the CO once for each staging_target_path that was
    // successfully setup via NodeStageVolume. If the corresponding
    // Controller Plugin has PUBLISH_UNPUBLISH_VOLUME controller capability and
    // the Node Plugin has STAGE_UNSTAGE_VOLUME capability, the CO MUST
    // guarantee that this RPC is called and returns success before calling
    // ControllerUnpublishVolume for the given node and the given volume. The CO
    // MUST guarantee that this RPC is called after all NodeUnpublishVolume have
    // been called and returned success for the given volume on the given node.
    // The Plugin SHALL assume that this RPC will be executed on the node where
    // the volume is being used. This RPC MAY be called by the CO when the
    // workload using the volume is being moved to a different node, or all the
    // workloads using the volume on a node have finished. This operation
    // MUST be idempotent. If the volume corresponding to the volume_id is not
    // staged to the staging_target_path, the Plugin MUST reply 0 OK.
    // If this RPC failed, or the CO does not know if it failed or not, it MAY
    // choose to call NodeUnstageVolume again.
    /// This operation MUST be idempotent. If the volume corresponding to the
    /// volume_id is not staged to the staging_target_path, the Plugin MUST
    /// reply 0 OK.
    fn node_unstage_volume(
        &mut self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Self::NodeUnstageVolumeFuture {
        let msg = request.into_inner();
        let volume_id = msg.volume_id.clone();
        let stage_path = msg.staging_target_path.clone();

        debug!("Unstaging volume {} at {}", volume_id, stage_path);

        let f = nbd::get_nbd_instance(&self.socket.clone(), &msg.volume_id)
            .and_then(move |nbd_disk| {
                if nbd_disk.is_none() {
                    // if we dont have a nbd device with a corresponding bdev,
                    // its an error ass it should
                    error!(
                        "No device instance found for {}, likely a bug",
                        &msg.volume_id
                    );

                    return err(Status::new(
                        Code::NotFound,
                        "no such bdev exists".to_string(),
                    ));
                }

                let nbd_disk = nbd_disk.unwrap();

                if let Some(mount) = match_mount(
                    Some(&nbd_disk.nbd_device),
                    Some(&msg.staging_target_path),
                    true,
                ) {
                    // we have an exact match unmount
                    if mount.source == nbd_disk.nbd_device
                        && msg.staging_target_path == mount.dest
                    {
                        return ok(true);
                    }
                }
                // staging does not match target path must reply OK
                ok(false)
            })
            .and_then(move |res| {
                if res {
                    if let Err(reason) = unmount_fs(&stage_path, false) {
                        dbg!(&reason);
                        grpc_return!(Code::Internal, reason);
                    }
                }
                Box::new(ok(Response::new(NodeUnstageVolumeResponse {})))
            });

        Box::new(f)
    }
}
