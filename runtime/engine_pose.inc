// Apply bounded local engine poses without changing received replication state.
struct EnginePoseWrite {
    Node* node=nullptr;
    Json before;
    Json after;
};

Json apply_engine_poses_impl(World& w,const Json& poses) {
    if(!poses.is_object()||poses.size()!=3||!poses.contains("character_id")||
       !poses.contains("root_id")||!poses.contains("frames"))
        throw std::runtime_error("invalid engine pose batch");
    if(!poses["character_id"].is_string()||!poses["root_id"].is_string()||
       !poses["frames"].is_array())
        throw std::runtime_error("invalid engine pose batch fields");
    const std::string character_id=poses["character_id"].get<std::string>();
    const std::string root_id=poses["root_id"].get<std::string>();
    const Json& frames=poses["frames"];
    if(character_id.empty()||root_id.empty()||frames.empty()||frames.size()>32)
        throw std::runtime_error("engine pose batch exceeds limits");

    Node* player=find_node(w,w.local_player);
    if(!player||player->destroyed||player->class_name!="Player")
        return Json{{"status","stale_character"},{"frames_applied",0}};
    const Json* character_ref=property_value(w,*player,"Character");
    if(!character_ref||!character_ref->is_object()||character_ref->size()!=1||
       !character_ref->contains("$ref")||!(*character_ref)["$ref"].is_string()||
       (*character_ref)["$ref"].get<std::string>()!=character_id)
        return Json{{"status","stale_character"},{"frames_applied",0}};
    Node* character=find_node(w,character_id);
    if(!character||character->destroyed||!pivot_model(w,*character))
        throw std::runtime_error("engine pose Character is unavailable");

    Node* root=find_node(w,root_id);
    if(!root||root->destroyed||!pivot_part(w,*root)||!is_descendant_of(w,*root,character_id))
        throw std::runtime_error("engine pose root is not a live Character BasePart");
    bool valid_root=instance_name(w,*root)=="HumanoidRootPart";
    if(const Json* primary=property_value(w,*character,"PrimaryPart"))
        valid_root=valid_root||(primary->is_object()&&primary->size()==1&&primary->contains("$ref")&&
            (*primary)["$ref"].is_string()&&(*primary)["$ref"].get<std::string>()==root_id);
    if(!valid_root)throw std::runtime_error("engine pose root is not HumanoidRootPart or Model.PrimaryPart");

    std::vector<EnginePoseWrite> writes;
    writes.reserve(frames.size());
    std::unordered_set<std::string> seen;
    bool root_included=false;
    for(const auto& frame:frames) {
        if(!frame.is_object()||frame.size()!=2||!frame.contains("id")||!frame.contains("cframe")||
           !frame["id"].is_string())throw std::runtime_error("invalid engine pose frame");
        const std::string id=frame["id"].get<std::string>();
        if(id.empty()||!seen.insert(id).second)throw std::runtime_error("duplicate or empty engine pose part id");
        Node* part=find_node(w,id);
        if(!part||part->destroyed||!pivot_part(w,*part)||!is_descendant_of(w,*part,character_id))
            throw std::runtime_error("engine pose frame target is not a live Character BasePart");
        const Json& cf=frame["cframe"];
        if(!cf.is_object()||cf.size()!=1||!cf.contains("$cframe"))
            throw std::runtime_error("engine pose frame requires canonical CFrame");
        const auto& body=cf["$cframe"];
        if(!body.is_object()||body.size()!=2||!body.contains("position")||!body.contains("rotation"))
            throw std::runtime_error("engine pose frame requires position and rotation");
        (void)pivot_decode(cf);
        EnginePoseWrite write;write.node=part;
        write.before=previous_part_transform(w,*part);
        write.after=part_fields_from_frame(cf);
        root_included=root_included||id==root_id;
        writes.push_back(std::move(write));
    }
    if(!root_included)throw std::runtime_error("engine pose batch is missing its root frame");

    // Rigidly joined parts the engine did not pose (accessory Handles on their
    // AccessoryWeld, welded tools and props) move with the part they are joined
    // to, keeping their offset; otherwise they stay where the character spawned.
    std::unordered_map<std::string,CFrameValue> motion;
    for(const auto& frame:frames) {
        Node* part=find_node(w,frame["id"].get<std::string>());
        try{motion[part->id]=pivot_product(pivot_decode(frame["cframe"]),pivot_inverse(pivot_frame(w,*part,"CFrame")));}
        catch(const std::exception&){}
    }
    std::vector<EnginePoseWrite> carried;
    try {
        JointGraph graph(w);
        std::deque<std::string> pending;
        for(const auto& [id,delta]:motion)pending.push_back(id);
        while(!pending.empty()) {
            const std::string from=pending.front();pending.pop_front();
            const CFrameValue delta=motion.at(from);
            for(auto index:graph.incident[from]) {
                const auto& edge=graph.edges[index];
                if(!edge.rigid||!edge.active||!edge.unknown.empty()||!edge.a||!edge.b)continue;
                Node* other=edge.a->id==from?edge.b:edge.a;
                if(!other||other->destroyed||seen.count(other->id)||motion.count(other->id)||!pivot_part(w,*other))continue;
                CFrameValue old;
                try{old=pivot_frame(w,*other,"CFrame");}catch(const std::exception&){continue;}
                motion[other->id]=delta;pending.push_back(other->id);
                EnginePoseWrite write;write.node=other;
                write.before=previous_part_transform(w,*other);
                write.after=part_fields_from_frame(pivot_encode(pivot_product(delta,old)));
                carried.push_back(std::move(write));
            }
        }
    }catch(const std::exception& error) {
        if(w.joint_pose_errors.size()<128)w.joint_pose_errors.push_back(Json{{"operation","engine_pose_carry"},{"error",error.what()}});
        carried.clear();
    }
    for(auto& write:carried)writes.push_back(std::move(write));

    // Validation and derivation are complete before any visible property changes.
    for(const auto& write:writes)
        for(auto it=write.after.begin();it!=write.after.end();++it)
            w.local_properties[write.node->id][it.key()]=it.value();
    for(const auto& write:writes)
        part_transform_events(w,*write.node,write.before,write.after);
    return Json{{"status","applied"},{"frames_applied",writes.size()-carried.size()},{"frames_carried",carried.size()}};
}
