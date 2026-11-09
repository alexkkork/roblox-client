// Rooted rigid-joint pose changes. This layer does not integrate forces or
// acquire simulation authority. Motor transforms apply at a scheduler boundary.
Node* assembly_root(World& w,Node& n,JointGraph& graph) {
    if(n.destroyed||joint_container(w,n).empty())return nullptr;
    auto candidates=graph.connected(n,true);candidates.push_back(&n);
    auto select=[&](auto score) {
        if(candidates.size()<2)return;
        double best=-std::numeric_limits<double>::infinity();std::vector<Node*> selected;
        for(auto* part:candidates) {
            double value=score(*part);
            if(value>best){best=value;selected.clear();}
            if(value==best)selected.push_back(part);
        }
        candidates=std::move(selected);
    };
    auto flag=[&](Node& part,const char* key) {
        auto* value=property_value(w,part,key);
        if(!value||!value->is_boolean())throw std::runtime_error(std::string("assembly root ")+key+" is unavailable");
        return value->get<bool>();
    };
    select([&](Node& p){return flag(p,"Anchored")?1.0:0.0;});
    select([&](Node& p){return flag(p,"Massless")?0.0:1.0;});
    select([&](Node& p){
        auto* v=property_value(w,p,"RootPriority");
        if(!v||!v->is_number())throw std::runtime_error("assembly RootPriority is unavailable");
        double value=v->get<double>();
        if(!std::isfinite(value)||value!=std::trunc(value)||value<-127||value>127)throw std::runtime_error("invalid assembly RootPriority");
        return value;
    });
    select([&](Node& p){
        auto* value=property_value(w,p,"Size");if(!value)throw std::runtime_error("assembly root Size is unavailable");
        auto size=part_vector(*value);for(auto n:size)if(n<=0)throw std::runtime_error("assembly root Size must be positive");
        double area=std::max({size[0]*size[1],size[1]*size[2],size[0]*size[2]});
        std::string name=instance_name(w,p);
        double multiplier=(class_is_a(w,p.class_name,"Seat")||p.class_name=="VehicleSeat")?20:name=="HumanoidRootPart"?10:name=="Torso"?5:1;
        double score=std::floor(area*50)*multiplier;
        if(!std::isfinite(score))throw std::runtime_error("assembly root sort size exceeds finite range");
        return score;
    });
    // Exact native tie ordering is unspecified; keep one local selection for
    // every query in this world, independent of which member was queried.
    std::unordered_set<std::string> ids;for(auto* p:candidates)ids.insert(p->id);
    for(const auto& id:w.order)if(ids.count(id))return find_node(w,id);
    throw std::runtime_error("assembly root is unavailable");
}
int get_assembly_root(lua_State* L) {
    auto* h=checked_handle(L,1);
    if(!pivot_part(*h->world,*h->node)){luaL_error(L,"GetRootPart requires BasePart");return 0;}
    try{JointGraph graph(*h->world);auto* root=assembly_root(*h->world,*h->node,graph);if(root)push_node(L,*h->world,root);else lua_pushnil(L);return 1;}
    catch(const std::exception& e){luaL_error(L,"assembly root unavailable: %s",e.what());return 0;}
}
bool assembly_root_index(lua_State* L,World& w,Node& n,const std::string& key) {
    if((key!="AssemblyRootPart"&&key!="GetRootPart")||!pivot_part(w,n))return false;
    if(key=="GetRootPart"){lua_pushcfunction(L,get_assembly_root,"GetRootPart");return true;}
    JointGraph graph(w);auto* root=assembly_root(w,n,graph);if(root)push_node(L,w,root);else lua_pushnil(L);return true;
}
std::string assembly_root_origin(World& w,const Node& n,const std::string& key) {
    if(key!="AssemblyRootPart"||!pivot_part(w,n))return {};
    try{JointGraph graph(w);assembly_root(w,const_cast<Node&>(n),graph);return "local";}catch(const std::exception&){return "unavailable";}
}
void watch_assembly_root(World& w,Node& n,const std::string& key) {
    if(key!="AssemblyRootPart"||!pivot_part(w,n)||w.assembly_root_watches.count(n.id))return;
    try{JointGraph graph(w);auto* root=assembly_root(w,n,graph);w.assembly_root_watches[n.id]=root?Json{{"$ref",root->id}}:Json(nullptr);}
    catch(const std::exception&){w.assembly_root_watches[n.id]=Json{{"unavailable",true}};}
}
void refresh_assembly_root_events(World& w) {
    if(w.assembly_root_watches.empty())return;JointGraph graph(w);
    for(auto it=w.assembly_root_watches.begin();it!=w.assembly_root_watches.end();) {
        auto* n=find_node(w,it->first);if(!n||n->destroyed){it=w.assembly_root_watches.erase(it);continue;}
        Json value;try{auto* root=assembly_root(w,*n,graph);if(root)value=Json{{"$ref",root->id}};}
        catch(const std::exception&){value=Json{{"unavailable",true}};}
        if(value!=it->second){it->second=value;property_events(w,*n,"AssemblyRootPart");}++it;
    }
}
using PoseChanges=std::vector<std::pair<Node*,Json>>;
CFrameValue joint_relative_frame(World& w,Node& joint,const Json& override_fields,bool pending) {
    if(joint.class_name=="Motor") {
        auto* angle=property_value(w,joint,"CurrentAngle");
        if(!angle||!angle->is_number()||angle->get<double>()!=0)throw std::runtime_error("legacy Motor angle solving is unavailable");
    }
    auto get=[&](const char* key) {
        if(override_fields.contains(key))return pivot_decode(override_fields[key]);
        if(!pending&&std::strcmp(key,"Transform")==0) {
            auto it=w.motor_applied_transforms.find(joint.id);
            if(it!=w.motor_applied_transforms.end())return pivot_decode(it->second);
        }
        return pivot_frame(w,joint,key);
    };
    auto c0=get("C0"),c1=get("C1");
    if(joint.class_name=="Motor6D")c0=pivot_product(c0,get("Transform"));
    return pivot_product(c0,pivot_inverse(c1));
}
PoseChanges prepare_joint_poses(World& w,JointGraph& graph,Node& root,const std::unordered_map<std::string,Json>& modified,bool pending,const std::unordered_set<std::string>& authoritative={}) {
    auto members=graph.connected(root,true);members.push_back(&root);
    std::unordered_map<std::string,CFrameValue> old,posed;
    for(auto* member:members)try{old.emplace(member->id,pivot_frame(w,*member,"CFrame"));}catch(const std::exception&){}
    auto previous=[&](Node& part)->const CFrameValue& {
        auto it=old.find(part.id);if(it==old.end())throw std::runtime_error("joint branch part CFrame is unavailable");return it->second;
    };
    posed[root.id]=previous(root);
    std::unordered_map<std::string,bool> moved{{root.id,false}};
    std::deque<Node*> queue{&root};PoseChanges result;
    while(!queue.empty()) {
        auto* parent=queue.front();queue.pop_front();
        for(auto index:graph.incident[parent->id]) {
            auto& edge=graph.edges[index];if(!edge.rigid||!edge.active)continue;
            auto* child=edge.a==parent?edge.b:edge.a;if(posed.count(child->id))continue;
            auto override_fields=modified.find(edge.joint->id);bool dirty=override_fields!=modified.end();
            // A received pose cuts inherited movement at this part. A dirty
            // joint further down the branch can still pose its own children.
            moved[child->id]=!authoritative.count(child->id)&&(dirty||moved[parent->id]);
            if(!moved[child->id])posed[child->id]=previous(*child);
            else {
                CFrameValue relative;
                if(dirty) {
                    relative=joint_relative_frame(w,*edge.joint,override_fields->second,pending);
                    if(edge.a!=parent)relative=pivot_inverse(relative);
                } else relative=pivot_product(pivot_inverse(previous(*parent)),previous(*child));
                auto next=pivot_product(posed.at(parent->id),relative);
                // Validate before committing any member of the affected batch.
                auto fields=part_fields_from_frame(pivot_encode(next));posed[child->id]=next;
                if(!old.count(child->id)||pivot_encode(next)!=pivot_encode(old.at(child->id)))result.emplace_back(child,std::move(fields));
            }
            queue.push_back(child);
        }
    }
    return result;
}
void commit_joint_poses(World& w,PoseChanges changes) {
    std::vector<Json> previous,entries;
    for(auto& [node,fields]:changes) {
        previous.push_back(pivot_part(w,*node)?previous_part_transform(w,*node):Json::object());
        for(auto it=fields.begin();it!=fields.end();++it) {
            entries.push_back(Json{{"kind","set"},{"id",node->id},{"property",it.key()},{"value",it.value()}});
            if(!pivot_part(w,*node))if(auto* old=property_value(w,*node,it.key()))previous.back()[it.key()]=*old;
        }
    }
    check_mutation_budget(w,entries);
    for(auto& [node,fields]:changes)for(auto it=fields.begin();it!=fields.end();++it)w.local_properties[node->id][it.key()]=it.value();
    append_mutations(w,std::move(entries));
    for(size_t i=0;i<changes.size();++i) {
        auto& [node,fields]=changes[i];if(pivot_part(w,*node))part_transform_events(w,*node,previous[i],fields);
        else for(auto it=fields.begin();it!=fields.end();++it)if(!previous[i].contains(it.key())||previous[i][it.key()]!=it.value())property_events(w,*node,it.key());
    }
}
bool joint_pose_write(lua_State* L,World& w,Node& n,const std::string& key) {
    if(!((key=="C0"||key=="C1")&&joint_rigid(n)&&n.class_name!="WeldConstraint")&&!(key=="Transform"&&n.class_name=="Motor6D"))return false;
    if(!has_meta(L,3,kCFrameMeta))throw std::runtime_error("joint pose must be a CFrame");
    auto value=convert_lua_value(L,w,3);pivot_decode(value);
    auto* old=property_value(w,n,key);Json before=old?*old:Json(nullptr);
    if(key=="Transform") {
        // Save the applied offset before storing a deferred target. Failure in
        // the later pose batch must not pretend that this target was applied.
        Json applied;bool save=!w.motor_applied_transforms.count(n.id);
        if(save)applied=before;
        commit_joint_poses(w,{{&n,Json{{key,value}}}});
        if(save)w.motor_applied_transforms[n.id]=std::move(applied);
        w.pending_motor_transforms.insert(n.id);return true;
    }
    if(before==value)return true;
    JointGraph graph(w);PoseChanges changes{{&n,Json{{key,value}}}};
    for(auto& edge:graph.edges)if(edge.joint==&n) {
        if(!edge.unknown.empty())throw std::runtime_error("joint pose unavailable: "+edge.unknown);
        if(edge.active) {
            auto* root=assembly_root(w,*edge.a,graph);
            if(!root)throw std::runtime_error("joint assembly root is unavailable");
            auto parts=prepare_joint_poses(w,graph,*root,{{n.id,Json{{key,value}}}},false);
            for(auto& item:parts)changes.push_back(std::move(item));
        }
        break;
    }
    commit_joint_poses(w,std::move(changes));return true;
}
void invalidate_joint_topology(World& w,Node& n,const std::string& key) {
    if(key=="Enabled"||key=="Part0"||key=="Part1") {
        if(!joint_rigid(n))return;
        w.active_pose_joints.erase(n.id);w.joint_topology_dirty=true;
    } else if(key=="Anchored"&&pivot_part(w,n))w.joint_topology_dirty=true;
    else if(key=="Parent") {
        w.joint_topology_dirty=true;
        // Invalidate affected edges even when a subtree leaves and re-enters
        // the same world within one scheduler turn.
        auto affected=[&](const std::string& id) {
            auto* node=find_node(w,id);return id==n.id||(node&&is_descendant_of(w,*node,n.id));
        };
        for(auto it=w.active_pose_joints.begin();it!=w.active_pose_joints.end();) {
            if(affected(it->first)||affected(it->second.first)||affected(it->second.second))it=w.active_pose_joints.erase(it);
            else ++it;
        }
    }
}
void collect_joint_activation(World& w,JointGraph& graph,std::unordered_set<std::string>& pending) {
    decltype(w.active_pose_joints) active;
    for(auto& edge:graph.edges)if(edge.active&&edge.rigid&&edge.joint->class_name!="WeldConstraint") {
        auto pair=std::make_pair(edge.a->id,edge.b->id);
        auto old=w.active_pose_joints.find(edge.joint->id);
        if(old==w.active_pose_joints.end()||old->second!=pair)pending.insert(edge.joint->id);
        active.emplace(edge.joint->id,std::move(pair));
    }
    w.active_pose_joints=std::move(active);w.joint_topology_dirty=false;
}
void seed_joint_activation(World& w) {
    JointGraph graph(w);std::unordered_set<std::string> ignored;collect_joint_activation(w,graph,ignored);
}
void remember_received_motor_transform(World& w,Node& n,const Json& fields) {
    if(n.class_name!="Motor6D"||!fields.contains("Transform")||w.motor_applied_transforms.count(n.id))return;
    auto* old=property_value(w,n,"Transform");w.motor_applied_transforms[n.id]=old?*old:Json(nullptr);
}
void commit_received_joint_poses(World& w,const PoseChanges& changes,uint64_t sequence) {
    // Derived received state is observable but is not an authored mutation to
    // echo back. All preparation precedes writes; events see the entire batch.
    std::vector<Json> before;for(auto& [part,fields]:changes)before.push_back(previous_part_transform(w,*part));
    for(auto& [part,fields]:changes)for(auto it=fields.begin();it!=fields.end();++it) {
        clear_local_property(w,part->id,it.key());part->properties[it.key()]=it.value();
        part->origins[it.key()]="derived_received";part->revisions[it.key()]=sequence;
    }
    for(size_t i=0;i<changes.size();++i)part_transform_events(w,*changes[i].first,before[i],changes[i].second);
}
void flush_joint_poses(World& w,std::unordered_set<std::string> pending,const std::unordered_set<std::string>& authoritative,uint64_t received_sequence=0) {
    if(pending.empty()&&w.pending_motor_transforms.empty()&&!w.joint_topology_dirty)return;
    const bool authored_motor=!w.pending_motor_transforms.empty();
    pending.insert(w.pending_motor_transforms.begin(),w.pending_motor_transforms.end());w.pending_motor_transforms.clear();
    try {
        JointGraph graph(w);std::unordered_map<std::string,Json> modified;
        if(w.joint_topology_dirty)collect_joint_activation(w,graph,pending);
        std::unordered_map<std::string,Node*> roots;
        for(const auto& id:pending) {
            auto* node=find_node(w,id);if(!node||node->destroyed)continue;
            for(auto& edge:graph.edges)if(edge.joint==node) {
                if(!edge.unknown.empty())throw std::runtime_error("motor pose unavailable: "+edge.unknown);
                if(edge.active) {
                    auto* root=assembly_root(w,*edge.a,graph);if(!root)throw std::runtime_error("motor assembly root is unavailable");
                    roots[root->id]=root;modified[id]=Json::object();
                }
                break;
            }
        }
        PoseChanges changes;
        // Stable order keeps mutation records deterministic across runs.
        for(const auto& id:w.order)if(roots.count(id)) {
            auto parts=prepare_joint_poses(w,graph,*roots.at(id),modified,true,authoritative);
            for(auto& item:parts)changes.push_back(std::move(item));
        }
        if(received_sequence&&!authored_motor)commit_received_joint_poses(w,changes,received_sequence);
        else commit_joint_poses(w,std::move(changes));
        for(const auto& id:pending)w.motor_applied_transforms.erase(id);
    }catch(const std::exception& error) {
        if(w.joint_pose_errors.size()<128)w.joint_pose_errors.push_back(Json{{"operation",authored_motor?"motor_transform_batch":"joint_pose_batch"},{"error",error.what()},{"joint_count",pending.size()}});
    }
}
void flush_motor_transforms(World& w){flush_joint_poses(w,{},{});}
// Physics replication for other players' characters (and other remote
// assemblies) carries only the assembly root's CFrame. Like joint_part_write
// does for local writes, move rigidly joined members with the root's delta,
// keeping their last solved offsets, unless the same batch gave them their own
// received CFrame. Without this, remote bodies stay where they were created.
void carry_received_assemblies(World& w,const std::unordered_map<std::string,CFrameValue>& before,
                               const std::unordered_set<std::string>& received,uint64_t sequence) {
    if(before.empty())return;
    try {
        JointGraph graph(w);PoseChanges changes;std::unordered_set<std::string> claimed;
        for(const auto& id:w.order) {
            auto previous=before.find(id);if(previous==before.end())continue;
            Node* root=find_node(w,id);if(!root||root->destroyed)continue;
            // Only an assembly root's pose carries the assembly. A received
            // pose on any other member is authoritative for that part alone.
            if(assembly_root(w,*root,graph)!=root)continue;
            CFrameValue now;
            try{now=pivot_frame(w,*root,"CFrame");}catch(const std::exception&){continue;}
            if(pivot_encode(now)==pivot_encode(previous->second))continue;
            auto delta=pivot_product(now,pivot_inverse(previous->second));
            for(auto* member:graph.connected(*root,true)) {
                if(received.count(member->id)||!claimed.insert(member->id).second)continue;
                CFrameValue old;
                try{old=pivot_frame(w,*member,"CFrame");}catch(const std::exception&){continue;}
                changes.emplace_back(member,part_fields_from_frame(pivot_encode(pivot_product(delta,old))));
            }
        }
        if(!changes.empty())commit_received_joint_poses(w,changes,sequence);
    }catch(const std::exception& error) {
        if(w.joint_pose_errors.size()<128)w.joint_pose_errors.push_back(Json{{"operation","received_assembly_carry"},{"error",error.what()}});
    }
}
