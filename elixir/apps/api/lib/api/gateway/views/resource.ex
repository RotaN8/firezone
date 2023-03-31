defmodule API.Gateway.Views.Resource do
  alias Domain.Resources

  def render(%Resources.Resource{type: :dns} = resource) do
    %{
      id: resource.id,
      type: :dns,
      address: resource.address,
      name: resource.name,
      ipv4: resource.ipv4,
      ipv6: resource.ipv6
    }
  end

  def render(%Resources.Resource{type: :cidr} = resource) do
    %{
      id: resource.id,
      type: :cidr,
      address: resource.address,
      name: resource.name
    }
  end
end
